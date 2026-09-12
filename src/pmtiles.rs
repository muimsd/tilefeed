//! PMTiles v3 writer.
//!
//! PMTiles packs a whole tileset into one file that can be served from static
//! hosting — S3, R2, a CDN — with clients fetching tiles by HTTP range request.
//! No tile server, no SQLite.
//!
//! Implemented from the v3 specification:
//! <https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md>
//!
//! The layout is: a fixed 127-byte header, the root directory, JSON metadata,
//! leaf directories (optional), then the tile data. Directories are varint
//! columns — all tile IDs, then all run lengths, then all lengths, then all
//! offsets — and are compressed. Tiles are addressed by a single `tile_id`
//! taken from a Hilbert curve over each zoom level, which keeps tiles that are
//! near each other on the map near each other in the file.

use anyhow::{bail, Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use std::collections::HashMap;
use std::io::Write;
use tracing::info;

use crate::mbtiles::MbtilesStore;

const MAGIC: &[u8; 7] = b"PMTiles";
const VERSION: u8 = 3;
pub const HEADER_LEN: usize = 127;

/// The header plus the compressed root directory must fit in 16384 bytes, so a
/// client can fetch both in one range request.
pub const MAX_ROOT_DIR_LEN: usize = 16384 - HEADER_LEN;

/// Compression enum (header bytes 97 and 98).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Compressed {
    None = 0x01,
    Gzip = 0x02,
}

/// Tile type enum (header byte 99).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TileType {
    Mvt = 0x01,
}

/// One directory entry: a tile, a run of identical tiles, or a leaf directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub tile_id: u64,
    /// Offset into the tile data section, or into the leaf section when this is
    /// a leaf pointer.
    pub offset: u64,
    pub length: u32,
    /// Number of consecutive tile IDs sharing this blob. `0` marks a pointer to
    /// a leaf directory rather than a tile.
    pub run_length: u32,
}

/// Tile ID for a tile coordinate: the tile's position along the Hilbert curve
/// for its zoom, offset past every tile of every lower zoom.
pub fn tile_id(z: u8, x: u32, y: u32) -> u64 {
    // Tiles below this zoom: 1 + 4 + 16 + ... = (4^z - 1) / 3
    let base: u64 = ((1u64 << (2 * z as u64)) - 1) / 3;

    let n = 1u64 << z;
    let (mut rx, mut ry): (u64, u64);
    let (mut tx, mut ty) = (x as u64, y as u64);
    let mut d: u64 = 0;
    let mut s = n / 2;

    while s > 0 {
        rx = if (tx & s) > 0 { 1 } else { 0 };
        ry = if (ty & s) > 0 { 1 } else { 0 };
        d += s * s * ((3 * rx) ^ ry);

        // Rotate the quadrant so the curve stays continuous
        if ry == 0 {
            if rx == 1 {
                tx = s.wrapping_sub(1).wrapping_sub(tx);
                ty = s.wrapping_sub(1).wrapping_sub(ty);
            }
            std::mem::swap(&mut tx, &mut ty);
        }
        s /= 2;
    }

    base + d
}

/// Inverse of [`tile_id`].
pub fn tile_id_to_zxy(id: u64) -> Result<(u8, u32, u32)> {
    let mut acc: u64 = 0;
    for z in 0u8..=31 {
        let tiles_at_zoom = 1u64 << (2 * z as u64);
        if id < acc + tiles_at_zoom {
            return Ok(hilbert_to_xy(z, id - acc));
        }
        acc += tiles_at_zoom;
    }
    bail!("tile id {} is beyond zoom 31", id)
}

fn hilbert_to_xy(z: u8, mut d: u64) -> (u8, u32, u32) {
    let n = 1u64 << z;
    let (mut x, mut y) = (0u64, 0u64);
    let mut s = 1u64;

    while s < n {
        let rx = 1 & (d / 2);
        let ry = 1 & (d ^ rx);

        if ry == 0 {
            if rx == 1 {
                x = s.wrapping_sub(1).wrapping_sub(x);
                y = s.wrapping_sub(1).wrapping_sub(y);
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        d /= 4;
        s *= 2;
    }

    (z, x as u32, y as u32)
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let byte = *buf.get(*pos).context("varint ran past end of directory")?;
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift > 63 {
            bail!("varint is too long to be a u64");
        }
    }
}

/// Serialize directory entries: a count, then one varint column per field.
pub fn serialize_directory(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 4);
    write_varint(&mut out, entries.len() as u64);

    // Tile IDs, delta-encoded
    let mut last_id = 0u64;
    for entry in entries {
        write_varint(&mut out, entry.tile_id - last_id);
        last_id = entry.tile_id;
    }

    for entry in entries {
        write_varint(&mut out, entry.run_length as u64);
    }
    for entry in entries {
        write_varint(&mut out, entry.length as u64);
    }

    // Offsets: 0 means "directly after the previous entry's blob", which is the
    // common case in a clustered archive and keeps the directory small.
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 && entry.offset == entries[i - 1].offset + entries[i - 1].length as u64 {
            write_varint(&mut out, 0);
        } else {
            write_varint(&mut out, entry.offset + 1);
        }
    }

    out
}

/// Parse a directory. Used by the tests to check round-tripping, and by
/// [`verify_archive`].
pub fn deserialize_directory(bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut pos = 0;
    let count = read_varint(bytes, &mut pos)? as usize;

    // A corrupt count must not make us try to allocate the world
    if count > 10_000_000 {
        bail!("directory claims {} entries, which is implausible", count);
    }

    let mut entries = Vec::with_capacity(count);
    let mut last_id = 0u64;
    for _ in 0..count {
        last_id += read_varint(bytes, &mut pos)?;
        entries.push(Entry {
            tile_id: last_id,
            offset: 0,
            length: 0,
            run_length: 0,
        });
    }
    for entry in entries.iter_mut() {
        entry.run_length = read_varint(bytes, &mut pos)? as u32;
    }
    for entry in entries.iter_mut() {
        entry.length = read_varint(bytes, &mut pos)? as u32;
    }
    for i in 0..count {
        let raw = read_varint(bytes, &mut pos)?;
        entries[i].offset = if raw == 0 {
            if i == 0 {
                bail!("first directory entry uses the contiguous-offset shorthand");
            }
            entries[i - 1].offset + entries[i - 1].length as u64
        } else {
            raw - 1
        };
    }

    Ok(entries)
}

/// The fixed 127-byte header.
#[derive(Debug, Clone)]
pub struct Header {
    pub root_offset: u64,
    pub root_length: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub leaf_offset: u64,
    pub leaf_length: u64,
    pub data_offset: u64,
    pub data_length: u64,
    pub addressed_tiles: u64,
    pub tile_entries: u64,
    pub tile_contents: u64,
    pub clustered: bool,
    pub internal_compression: Compressed,
    pub tile_compression: Compressed,
    pub tile_type: TileType,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// (min_lon, min_lat, max_lon, max_lat) in degrees
    pub bounds: (f64, f64, f64, f64),
    pub center_zoom: u8,
    pub center: (f64, f64),
}

/// Positions are stored as degrees × 10,000,000 in a little-endian i32.
fn position(degrees: f64) -> [u8; 4] {
    ((degrees * 10_000_000.0).round() as i32).to_le_bytes()
}

fn position_from(bytes: &[u8]) -> f64 {
    i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64 / 10_000_000.0
}

impl Header {
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..7].copy_from_slice(MAGIC);
        out[7] = VERSION;
        out[8..16].copy_from_slice(&self.root_offset.to_le_bytes());
        out[16..24].copy_from_slice(&self.root_length.to_le_bytes());
        out[24..32].copy_from_slice(&self.metadata_offset.to_le_bytes());
        out[32..40].copy_from_slice(&self.metadata_length.to_le_bytes());
        out[40..48].copy_from_slice(&self.leaf_offset.to_le_bytes());
        out[48..56].copy_from_slice(&self.leaf_length.to_le_bytes());
        out[56..64].copy_from_slice(&self.data_offset.to_le_bytes());
        out[64..72].copy_from_slice(&self.data_length.to_le_bytes());
        out[72..80].copy_from_slice(&self.addressed_tiles.to_le_bytes());
        out[80..88].copy_from_slice(&self.tile_entries.to_le_bytes());
        out[88..96].copy_from_slice(&self.tile_contents.to_le_bytes());
        out[96] = u8::from(self.clustered);
        out[97] = self.internal_compression as u8;
        out[98] = self.tile_compression as u8;
        out[99] = self.tile_type as u8;
        out[100] = self.min_zoom;
        out[101] = self.max_zoom;
        out[102..106].copy_from_slice(&position(self.bounds.0));
        out[106..110].copy_from_slice(&position(self.bounds.1));
        out[110..114].copy_from_slice(&position(self.bounds.2));
        out[114..118].copy_from_slice(&position(self.bounds.3));
        out[118] = self.center_zoom;
        out[119..123].copy_from_slice(&position(self.center.0));
        out[123..127].copy_from_slice(&position(self.center.1));
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            bail!(
                "PMTiles header is {} bytes, expected {}",
                bytes.len(),
                HEADER_LEN
            );
        }
        if &bytes[0..7] != MAGIC {
            bail!("not a PMTiles archive (bad magic number)");
        }
        if bytes[7] != VERSION {
            bail!("PMTiles version {} is not supported (expected 3)", bytes[7]);
        }

        let u64_at = |i: usize| {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[i..i + 8]);
            u64::from_le_bytes(buf)
        };

        Ok(Header {
            root_offset: u64_at(8),
            root_length: u64_at(16),
            metadata_offset: u64_at(24),
            metadata_length: u64_at(32),
            leaf_offset: u64_at(40),
            leaf_length: u64_at(48),
            data_offset: u64_at(56),
            data_length: u64_at(64),
            addressed_tiles: u64_at(72),
            tile_entries: u64_at(80),
            tile_contents: u64_at(88),
            clustered: bytes[96] == 0x01,
            internal_compression: match bytes[97] {
                0x01 => Compressed::None,
                0x02 => Compressed::Gzip,
                other => bail!("unsupported internal compression {}", other),
            },
            tile_compression: match bytes[98] {
                0x01 => Compressed::None,
                0x02 => Compressed::Gzip,
                other => bail!("unsupported tile compression {}", other),
            },
            tile_type: match bytes[99] {
                0x01 => TileType::Mvt,
                other => bail!("unsupported tile type {}", other),
            },
            min_zoom: bytes[100],
            max_zoom: bytes[101],
            bounds: (
                position_from(&bytes[102..106]),
                position_from(&bytes[106..110]),
                position_from(&bytes[110..114]),
                position_from(&bytes[114..118]),
            ),
            center_zoom: bytes[118],
            center: (
                position_from(&bytes[119..123]),
                position_from(&bytes[123..127]),
            ),
        })
    }
}

fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// Split entries into a root directory and leaf directories small enough that
/// the root fits alongside the header in one 16 KiB range request.
///
/// Returns (compressed root, concatenated compressed leaves, leaf count).
fn build_directories(entries: &[Entry]) -> Result<(Vec<u8>, Vec<u8>, usize)> {
    let root = gzip(&serialize_directory(entries))?;
    if root.len() <= MAX_ROOT_DIR_LEN {
        return Ok((root, Vec::new(), 0));
    }

    // Halve the leaf size until the root fits. Each leaf holds a slice of the
    // entries; the root then holds one run_length=0 pointer per leaf.
    let mut leaf_size = 4096;
    loop {
        let mut root_entries = Vec::new();
        let mut leaves = Vec::new();

        for chunk in entries.chunks(leaf_size) {
            let leaf = gzip(&serialize_directory(chunk))?;
            root_entries.push(Entry {
                tile_id: chunk[0].tile_id,
                offset: leaves.len() as u64,
                length: leaf.len() as u32,
                run_length: 0,
            });
            leaves.extend_from_slice(&leaf);
        }

        let root = gzip(&serialize_directory(&root_entries))?;
        if root.len() <= MAX_ROOT_DIR_LEN {
            return Ok((root, leaves, root_entries.len()));
        }

        leaf_size *= 2;
        if leaf_size > entries.len() {
            bail!(
                "cannot fit a root directory for {} tile entries into {} bytes",
                entries.len(),
                MAX_ROOT_DIR_LEN
            );
        }
    }
}

/// What an export produced, for reporting and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportStats {
    /// Tiles addressable in the archive
    pub addressed_tiles: u64,
    /// Directory entries (fewer than tiles when runs are collapsed)
    pub tile_entries: u64,
    /// Distinct tile blobs written (fewer than entries when tiles are deduplicated)
    pub tile_contents: u64,
    pub leaf_directories: usize,
    pub bytes_written: u64,
    pub min_zoom: u8,
    pub max_zoom: u8,
}

/// Convert an MBTiles file into a PMTiles archive.
pub fn export_mbtiles(input: &str, output: &str) -> Result<ExportStats> {
    let store = MbtilesStore::open(input)?;
    let metadata: HashMap<String, String> = store.get_all_metadata()?.into_iter().collect();

    let raw_coords = store.all_tile_coords()?;
    if raw_coords.is_empty() {
        bail!("{} contains no tiles", input);
    }

    // MBTiles rows are TMS, PMTiles tile IDs are XYZ. Flipping y here and
    // reading through `get_tile_raw_tms` keeps exactly one conversion in play.
    let mut coords: Vec<(u64, u8, u32, u32)> = raw_coords
        .into_iter()
        .map(|(z, x, tms_y)| {
            let y = (1u32 << z) - 1 - tms_y;
            (tile_id(z, x, y), z, x, tms_y)
        })
        .collect();

    // Hilbert order: what makes the archive clustered, and what lets nearby
    // tiles share a range request.
    coords.sort_by_key(|(id, _, _, _)| *id);

    let min_zoom = coords.iter().map(|(_, z, _, _)| *z).min().unwrap_or(0);
    let max_zoom = coords.iter().map(|(_, z, _, _)| *z).max().unwrap_or(0);

    let mut entries: Vec<Entry> = Vec::new();
    let mut tile_data: Vec<u8> = Vec::new();
    // Content hash -> (offset, length), so a repeated tile is stored once
    let mut seen: HashMap<[u8; 32], (u64, u32)> = HashMap::new();
    let mut addressed_tiles: u64 = 0;
    let mut tile_compression: Option<Compressed> = None;

    for (id, z, x, tms_y) in &coords {
        let data = match store.get_tile_raw_tms(*z, *x, *tms_y)? {
            Some(data) => data,
            None => continue, // deleted between listing and reading
        };

        if tile_compression.is_none() {
            tile_compression = Some(if crate::mvt::is_gzipped(&data) {
                Compressed::Gzip
            } else {
                Compressed::None
            });
        }

        let id = *id;
        addressed_tiles += 1;

        let hash: [u8; 32] = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&data);
            hasher.finalize().into()
        };

        // A run: the tile right before this one, with identical content
        if let Some(last) = entries.last_mut() {
            if last.tile_id + last.run_length as u64 == id
                && seen.get(&hash) == Some(&(last.offset, last.length))
            {
                last.run_length += 1;
                continue;
            }
        }

        match seen.get(&hash) {
            Some(&(offset, length)) => {
                entries.push(Entry {
                    tile_id: id,
                    offset,
                    length,
                    run_length: 1,
                });
            }
            None => {
                let offset = tile_data.len() as u64;
                let length = data.len() as u32;
                tile_data.extend_from_slice(&data);
                seen.insert(hash, (offset, length));
                entries.push(Entry {
                    tile_id: id,
                    offset,
                    length,
                    run_length: 1,
                });
            }
        }
    }

    if entries.is_empty() {
        bail!("{} contains no readable tiles", input);
    }

    let (root, leaves, leaf_count) = build_directories(&entries)?;
    let metadata_json = build_metadata(&metadata)?;
    let metadata_bytes = gzip(metadata_json.as_bytes())?;

    let root_offset = HEADER_LEN as u64;
    let metadata_offset = root_offset + root.len() as u64;
    let leaf_offset = metadata_offset + metadata_bytes.len() as u64;
    let data_offset = leaf_offset + leaves.len() as u64;

    let header = Header {
        root_offset,
        root_length: root.len() as u64,
        metadata_offset,
        metadata_length: metadata_bytes.len() as u64,
        leaf_offset,
        leaf_length: leaves.len() as u64,
        data_offset,
        data_length: tile_data.len() as u64,
        addressed_tiles,
        tile_entries: entries.len() as u64,
        tile_contents: seen.len() as u64,
        clustered: true,
        internal_compression: Compressed::Gzip,
        tile_compression: tile_compression.unwrap_or(Compressed::None),
        tile_type: TileType::Mvt,
        min_zoom,
        max_zoom,
        bounds: parse_bounds(metadata.get("bounds")),
        center_zoom: parse_center_zoom(metadata.get("center")).unwrap_or(min_zoom),
        center: parse_center(metadata.get("center")),
    };

    let mut file = Vec::with_capacity(
        HEADER_LEN + root.len() + metadata_bytes.len() + leaves.len() + tile_data.len(),
    );
    file.extend_from_slice(&header.to_bytes());
    file.extend_from_slice(&root);
    file.extend_from_slice(&metadata_bytes);
    file.extend_from_slice(&leaves);
    file.extend_from_slice(&tile_data);

    std::fs::write(output, &file)
        .with_context(|| format!("Failed to write PMTiles archive to {}", output))?;

    Ok(ExportStats {
        addressed_tiles,
        tile_entries: entries.len() as u64,
        tile_contents: seen.len() as u64,
        leaf_directories: leaf_count,
        bytes_written: file.len() as u64,
        min_zoom,
        max_zoom,
    })
}

/// PMTiles metadata is a JSON object. MBTiles keeps most of it as flat strings
/// plus a `json` blob holding `vector_layers`; flatten that blob in, which is
/// where clients look for layer definitions.
fn build_metadata(metadata: &HashMap<String, String>) -> Result<String> {
    let mut out = serde_json::Map::new();

    for (key, value) in metadata {
        if key == "json" {
            continue;
        }
        out.insert(key.clone(), serde_json::Value::String(value.clone()));
    }

    if let Some(raw) = metadata.get("json") {
        if let Ok(serde_json::Value::Object(nested)) = serde_json::from_str(raw) {
            for (key, value) in nested {
                out.insert(key, value);
            }
        }
    }

    Ok(serde_json::to_string(&serde_json::Value::Object(out))?)
}

/// Web Mercator's latitude limit; the default when an MBTiles has no bounds.
const MAX_LAT: f64 = 85.051_128_78;

fn parse_bounds(raw: Option<&String>) -> (f64, f64, f64, f64) {
    let parsed = raw.and_then(|s| {
        let parts: Vec<f64> = s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        match parts[..] {
            [min_lon, min_lat, max_lon, max_lat] => Some((min_lon, min_lat, max_lon, max_lat)),
            _ => None,
        }
    });
    parsed.unwrap_or((-180.0, -MAX_LAT, 180.0, MAX_LAT))
}

fn parse_center(raw: Option<&String>) -> (f64, f64) {
    let parsed = raw.and_then(|s| {
        let parts: Vec<f64> = s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        match parts[..] {
            [lon, lat] | [lon, lat, _] => Some((lon, lat)),
            _ => None,
        }
    });
    parsed.unwrap_or((0.0, 0.0))
}

fn parse_center_zoom(raw: Option<&String>) -> Option<u8> {
    let parts: Vec<&str> = raw?.split(',').collect();
    parts.get(2)?.trim().parse::<f64>().ok().map(|z| z as u8)
}

/// Read an archive back and check it describes itself consistently: the header
/// parses, the directories parse, and every entry points inside the tile data.
///
/// Used by `export` to refuse to leave a corrupt archive behind, and by tests.
pub fn verify_archive(bytes: &[u8]) -> Result<Header> {
    let header = Header::from_bytes(bytes)?;

    // Every section the header declares has to be inside the file. Checking the
    // entries against `data_length` alone would pass on a truncated archive,
    // since that length is itself just a claim in the header.
    let declared_end = header.data_offset + header.data_length;
    if declared_end > bytes.len() as u64 {
        bail!(
            "archive is {} bytes but the header describes {}",
            bytes.len(),
            declared_end
        );
    }

    let root_end = (header.root_offset + header.root_length) as usize;
    if root_end > bytes.len() {
        bail!("root directory extends past the end of the archive");
    }
    let root = ungzip(&bytes[header.root_offset as usize..root_end])?;
    let root_entries = deserialize_directory(&root)?;

    let mut tile_entries = Vec::new();
    for entry in &root_entries {
        if entry.run_length == 0 {
            let start = (header.leaf_offset + entry.offset) as usize;
            let end = start + entry.length as usize;
            if end > bytes.len() {
                bail!("leaf directory extends past the end of the archive");
            }
            let leaf = ungzip(&bytes[start..end])?;
            tile_entries.extend(deserialize_directory(&leaf)?);
        } else {
            tile_entries.push(entry.clone());
        }
    }

    for entry in &tile_entries {
        if entry.offset + entry.length as u64 > header.data_length {
            bail!(
                "tile {} points outside the tile data section",
                entry.tile_id
            );
        }
    }

    if tile_entries.len() as u64 != header.tile_entries {
        bail!(
            "header says {} tile entries, directories hold {}",
            header.tile_entries,
            tile_entries.len()
        );
    }

    Ok(header)
}

fn ungzip(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut out = Vec::new();
    GzDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

/// Read one tile out of an archive, resolving leaf directories.
/// Used by the tests to prove tiles survive the round trip.
pub fn read_tile(bytes: &[u8], z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>> {
    let header = Header::from_bytes(bytes)?;
    let wanted = tile_id(z, x, y);

    let root_end = (header.root_offset + header.root_length) as usize;
    let root = ungzip(&bytes[header.root_offset as usize..root_end])?;
    let mut entries = deserialize_directory(&root)?;

    // At most one level of leaf directories, as the spec recommends
    for _ in 0..2 {
        let found = entries.iter().rev().find(|e| e.tile_id <= wanted).cloned();

        let entry = match found {
            Some(entry) => entry,
            None => return Ok(None),
        };

        if entry.run_length == 0 {
            let start = (header.leaf_offset + entry.offset) as usize;
            let end = start + entry.length as usize;
            entries = deserialize_directory(&ungzip(&bytes[start..end])?)?;
            continue;
        }

        if wanted >= entry.tile_id + entry.run_length as u64 {
            return Ok(None);
        }

        let start = (header.data_offset + entry.offset) as usize;
        let end = start + entry.length as usize;
        return Ok(Some(bytes[start..end].to_vec()));
    }

    Ok(None)
}

/// CLI entry point for `tilefeed export`.
pub fn export(input: &str, output: &str) -> Result<()> {
    let stats = export_mbtiles(input, output)?;

    // Read it back before declaring success: a silently malformed archive would
    // only show up in a browser, somewhere else, later.
    let written = std::fs::read(output)?;
    verify_archive(&written).with_context(|| format!("{} failed verification", output))?;

    info!("Wrote {} ({} bytes)", output, stats.bytes_written);
    println!("PMTiles archive: {}", output);
    println!();
    println!("  Zoom levels:      {}-{}", stats.min_zoom, stats.max_zoom);
    println!("  Tiles:            {}", stats.addressed_tiles);
    println!(
        "  Directory entries: {} ({} saved by runs)",
        stats.tile_entries,
        stats.addressed_tiles - stats.tile_entries
    );
    println!(
        "  Distinct blobs:   {} ({} deduplicated)",
        stats.tile_contents,
        stats.tile_entries - stats.tile_contents
    );
    println!("  Leaf directories: {}", stats.leaf_directories);
    println!("  Size:             {} bytes", stats.bytes_written);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- tile ids ---

    #[test]
    fn test_tile_id_matches_the_spec_examples() {
        // From the PMTiles v3 specification's reference table
        assert_eq!(tile_id(0, 0, 0), 0);
        assert_eq!(tile_id(1, 0, 0), 1);
        assert_eq!(tile_id(1, 0, 1), 2);
        assert_eq!(tile_id(1, 1, 1), 3);
        assert_eq!(tile_id(1, 1, 0), 4);
        assert_eq!(tile_id(2, 0, 0), 5);
        assert_eq!(tile_id(12, 3423, 1763), 19078479);
    }

    #[test]
    fn test_tile_id_round_trips() {
        for z in 0u8..=10 {
            let n = 1u32 << z;
            for (x, y) in [
                (0, 0),
                (n - 1, 0),
                (0, n - 1),
                (n - 1, n - 1),
                (n / 2, n / 3),
            ] {
                let id = tile_id(z, x, y);
                assert_eq!(
                    tile_id_to_zxy(id).unwrap(),
                    (z, x, y),
                    "round trip failed for {}/{}/{}",
                    z,
                    x,
                    y
                );
            }
        }
    }

    #[test]
    fn test_tile_ids_are_unique_and_contiguous_within_a_zoom() {
        // Every tile at z=4 must map to a distinct id, and the ids must exactly
        // fill the range reserved for that zoom.
        let mut ids: Vec<u64> = Vec::new();
        for x in 0..16 {
            for y in 0..16 {
                ids.push(tile_id(4, x, y));
            }
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 256);
        assert_eq!(*ids.first().unwrap(), tile_id(4, 0, 0));
        assert_eq!(*ids.last().unwrap(), tile_id(4, 0, 0) + 255);
    }

    #[test]
    fn test_zoom_bases_do_not_overlap() {
        // The last id of one zoom must be one less than the first of the next
        for z in 0u8..12 {
            let first_of_next = (1u64 << (2 * (z as u64 + 1))) / 3 + 1;
            let base_next = ((1u64 << (2 * (z as u64 + 1))) - 1) / 3;
            assert_eq!(base_next + 1, first_of_next);
        }
    }

    // --- varints ---

    #[test]
    fn test_varint_round_trip() {
        for value in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            write_varint(&mut buf, value);
            let mut pos = 0;
            assert_eq!(read_varint(&buf, &mut pos).unwrap(), value);
            assert_eq!(pos, buf.len(), "varint for {} had trailing bytes", value);
        }
    }

    #[test]
    fn test_varint_encoding_is_leb128() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 300);
        assert_eq!(buf, vec![0xAC, 0x02]);
    }

    #[test]
    fn test_truncated_varint_errors() {
        let mut pos = 0;
        assert!(read_varint(&[0x80], &mut pos).is_err());
    }

    // --- directories ---

    fn sample_entries() -> Vec<Entry> {
        vec![
            Entry {
                tile_id: 0,
                offset: 0,
                length: 10,
                run_length: 1,
            },
            // Contiguous: encodes its offset as 0
            Entry {
                tile_id: 1,
                offset: 10,
                length: 20,
                run_length: 3,
            },
            // Deduplicated: points back at the first blob
            Entry {
                tile_id: 9,
                offset: 0,
                length: 10,
                run_length: 1,
            },
        ]
    }

    #[test]
    fn test_directory_round_trip() {
        let entries = sample_entries();
        let bytes = serialize_directory(&entries);
        assert_eq!(deserialize_directory(&bytes).unwrap(), entries);
    }

    #[test]
    fn test_directory_uses_the_contiguous_offset_shorthand() {
        let entries = sample_entries();
        let bytes = serialize_directory(&entries);

        // Walk to the offsets column and check the middle entry encoded as 0
        let mut pos = 0;
        let count = read_varint(&bytes, &mut pos).unwrap() as usize;
        for _ in 0..count * 3 {
            read_varint(&bytes, &mut pos).unwrap();
        }
        assert_eq!(read_varint(&bytes, &mut pos).unwrap(), 1); // offset 0 -> 0+1
        assert_eq!(read_varint(&bytes, &mut pos).unwrap(), 0); // contiguous
        assert_eq!(read_varint(&bytes, &mut pos).unwrap(), 1); // back-reference
    }

    #[test]
    fn test_empty_directory_round_trips() {
        let bytes = serialize_directory(&[]);
        assert_eq!(deserialize_directory(&bytes).unwrap(), Vec::<Entry>::new());
    }

    #[test]
    fn test_directory_rejects_implausible_count() {
        let mut bytes = Vec::new();
        write_varint(&mut bytes, u64::MAX);
        assert!(deserialize_directory(&bytes).is_err());
    }

    // --- header ---

    fn sample_header() -> Header {
        Header {
            root_offset: 127,
            root_length: 50,
            metadata_offset: 177,
            metadata_length: 30,
            leaf_offset: 207,
            leaf_length: 0,
            data_offset: 207,
            data_length: 900,
            addressed_tiles: 12,
            tile_entries: 10,
            tile_contents: 8,
            clustered: true,
            internal_compression: Compressed::Gzip,
            tile_compression: Compressed::Gzip,
            tile_type: TileType::Mvt,
            min_zoom: 0,
            max_zoom: 14,
            bounds: (-0.1425, 51.5007, -0.0745, 51.5142),
            center_zoom: 14,
            center: (-0.076904, 51.501904),
        }
    }

    #[test]
    fn test_header_is_127_bytes_with_the_right_magic() {
        let bytes = sample_header().to_bytes();
        assert_eq!(bytes.len(), 127);
        assert_eq!(&bytes[0..7], b"PMTiles");
        assert_eq!(bytes[7], 3);
    }

    #[test]
    fn test_header_round_trip() {
        let header = sample_header();
        let parsed = Header::from_bytes(&header.to_bytes()).unwrap();

        assert_eq!(parsed.root_offset, header.root_offset);
        assert_eq!(parsed.data_length, header.data_length);
        assert_eq!(parsed.addressed_tiles, header.addressed_tiles);
        assert_eq!(parsed.tile_entries, header.tile_entries);
        assert_eq!(parsed.tile_contents, header.tile_contents);
        assert!(parsed.clustered);
        assert_eq!(parsed.min_zoom, 0);
        assert_eq!(parsed.max_zoom, 14);
        // Positions survive to 1e-7 degrees, the format's resolution
        assert!((parsed.bounds.0 - header.bounds.0).abs() < 1e-7);
        assert!((parsed.bounds.3 - header.bounds.3).abs() < 1e-7);
        assert!((parsed.center.1 - header.center.1).abs() < 1e-7);
    }

    #[test]
    fn test_header_fields_land_on_the_spec_offsets() {
        let bytes = sample_header().to_bytes();
        // Root directory offset is bytes 8..16, little-endian
        assert_eq!(&bytes[8..16], &127u64.to_le_bytes());
        // Clustered flag, compressions and tile type
        assert_eq!(bytes[96], 0x01);
        assert_eq!(bytes[97], 0x02);
        assert_eq!(bytes[98], 0x02);
        assert_eq!(bytes[99], 0x01);
        // min/max zoom
        assert_eq!(bytes[100], 0);
        assert_eq!(bytes[101], 14);
    }

    #[test]
    fn test_header_rejects_bad_magic() {
        let mut bytes = sample_header().to_bytes();
        bytes[0] = b'X';
        assert!(Header::from_bytes(&bytes).is_err());
    }

    #[test]
    fn test_header_rejects_wrong_version() {
        let mut bytes = sample_header().to_bytes();
        bytes[7] = 2;
        let err = Header::from_bytes(&bytes).unwrap_err().to_string();
        assert!(err.contains("version 2"), "got: {}", err);
    }

    #[test]
    fn test_header_rejects_short_input() {
        assert!(Header::from_bytes(&[0u8; 10]).is_err());
    }

    // --- end to end ---

    fn temp_path(suffix: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir()
            .join(format!(
                "tilefeed-pmtiles-{}-{}{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst),
                suffix
            ))
            .to_string_lossy()
            .to_string()
    }

    /// An MBTiles with a handful of tiles, two of them identical.
    fn fixture_mbtiles() -> String {
        let path = temp_path(".mbtiles");
        let store = MbtilesStore::create(&path).unwrap();
        store
            .write_default_metadata("fixture", "test fixture")
            .unwrap();
        store.set_metadata("bounds", "-1.0,50.0,1.0,52.0").unwrap();
        store.set_metadata("center", "0.0,51.0,3").unwrap();
        store
            .set_metadata("json", r#"{"vector_layers":[{"id":"parks"}]}"#)
            .unwrap();

        store.put_tile(0, 0, 0, b"tile-zero").unwrap();
        store.put_tile(1, 0, 0, b"tile-a").unwrap();
        store.put_tile(1, 1, 0, b"tile-b").unwrap();
        // Same bytes as 1/0/0: must be stored once
        store.put_tile(1, 1, 1, b"tile-a").unwrap();
        store.put_tile(2, 1, 1, b"tile-c").unwrap();
        path
    }

    #[test]
    fn test_export_round_trips_every_tile() {
        let input = fixture_mbtiles();
        let output = temp_path(".pmtiles");

        let stats = export_mbtiles(&input, &output).unwrap();
        assert_eq!(stats.addressed_tiles, 5);
        // "tile-a" appears twice but is stored once
        assert_eq!(stats.tile_contents, 4);
        assert_eq!(stats.min_zoom, 0);
        assert_eq!(stats.max_zoom, 2);

        let bytes = std::fs::read(&output).unwrap();
        verify_archive(&bytes).unwrap();

        for (z, x, y, expected) in [
            (0u8, 0u32, 0u32, &b"tile-zero"[..]),
            (1, 0, 0, &b"tile-a"[..]),
            (1, 1, 0, &b"tile-b"[..]),
            (1, 1, 1, &b"tile-a"[..]),
            (2, 1, 1, &b"tile-c"[..]),
        ] {
            assert_eq!(
                read_tile(&bytes, z, x, y).unwrap().as_deref(),
                Some(expected),
                "tile {}/{}/{} did not survive the round trip",
                z,
                x,
                y
            );
        }

        // A tile that was never written
        assert_eq!(read_tile(&bytes, 2, 0, 0).unwrap(), None);

        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
    }

    #[test]
    fn test_export_deduplicates_identical_tiles() {
        let input = temp_path(".mbtiles");
        let output = temp_path(".pmtiles");
        {
            let store = MbtilesStore::create(&input).unwrap();
            // Every tile at z=2 identical: one blob, and consecutive ids collapse
            // into runs rather than 16 separate entries.
            for x in 0..4 {
                for y in 0..4 {
                    store.put_tile(2, x, y, b"identical").unwrap();
                }
            }
        }

        let stats = export_mbtiles(&input, &output).unwrap();
        assert_eq!(stats.addressed_tiles, 16);
        assert_eq!(stats.tile_contents, 1, "one distinct blob expected");
        assert_eq!(
            stats.tile_entries, 1,
            "16 consecutive ids collapse to one run"
        );

        let bytes = std::fs::read(&output).unwrap();
        for x in 0..4 {
            for y in 0..4 {
                assert_eq!(
                    read_tile(&bytes, 2, x, y).unwrap().as_deref(),
                    Some(&b"identical"[..])
                );
            }
        }

        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
    }

    #[test]
    fn test_export_writes_header_metadata_from_mbtiles() {
        let input = fixture_mbtiles();
        let output = temp_path(".pmtiles");
        export_mbtiles(&input, &output).unwrap();

        let bytes = std::fs::read(&output).unwrap();
        let header = Header::from_bytes(&bytes).unwrap();

        assert!((header.bounds.0 - -1.0).abs() < 1e-7);
        assert!((header.bounds.3 - 52.0).abs() < 1e-7);
        assert_eq!(header.center_zoom, 3);
        assert!(header.clustered);
        assert_eq!(header.tile_type, TileType::Mvt);
        // Plain bytes in the fixture, so no compression is claimed
        assert_eq!(header.tile_compression, Compressed::None);

        let meta_start = header.metadata_offset as usize;
        let meta_end = meta_start + header.metadata_length as usize;
        let meta: serde_json::Value =
            serde_json::from_slice(&ungzip(&bytes[meta_start..meta_end]).unwrap()).unwrap();
        assert_eq!(meta["name"], "fixture");
        assert_eq!(meta["vector_layers"][0]["id"], "parks");

        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
    }

    #[test]
    fn test_build_directories_falls_back_to_leaves() {
        // A directory only needs leaves once it stops compressing: scattered ids
        // and varied lengths defeat gzip the way a real sparse tileset does.
        let mut state = 0x2545F4914F6CDD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut entries = Vec::new();
        let mut id = 0u64;
        let mut offset = 0u64;
        for _ in 0..60_000 {
            id += 1 + next() % 4096;
            let length = (1000 + next() % 60000) as u32;
            entries.push(Entry {
                tile_id: id,
                offset,
                length,
                run_length: 1,
            });
            // Non-contiguous, so the offset column cannot use the shorthand
            offset += length as u64 + next() % 7;
        }

        let (root, leaves, leaf_count) = build_directories(&entries).unwrap();
        assert!(leaf_count > 0, "this directory should not fit in a root");
        assert!(
            root.len() <= MAX_ROOT_DIR_LEN,
            "root is {} bytes, over the {} limit",
            root.len(),
            MAX_ROOT_DIR_LEN
        );
        assert!(!leaves.is_empty());

        // Reassembling root + leaves must give back exactly the input
        let root_entries = deserialize_directory(&ungzip(&root).unwrap()).unwrap();
        assert_eq!(root_entries.len(), leaf_count);
        let mut rebuilt = Vec::new();
        for pointer in &root_entries {
            assert_eq!(pointer.run_length, 0, "root entries must point at leaves");
            let start = pointer.offset as usize;
            let end = start + pointer.length as usize;
            rebuilt.extend(deserialize_directory(&ungzip(&leaves[start..end]).unwrap()).unwrap());
        }
        assert_eq!(rebuilt, entries);
    }

    #[test]
    fn test_build_directories_keeps_a_compressible_directory_in_the_root() {
        // Consecutive ids, uniform lengths, contiguous offsets: the columns are
        // almost all the same varint, so even a large tileset stays root-only.
        let entries: Vec<Entry> = (0..50_000u64)
            .map(|i| Entry {
                tile_id: i,
                offset: i * 100,
                length: 100,
                run_length: 1,
            })
            .collect();

        let (root, leaves, leaf_count) = build_directories(&entries).unwrap();
        assert_eq!(leaf_count, 0);
        assert!(leaves.is_empty());
        assert!(root.len() <= MAX_ROOT_DIR_LEN);
    }

    #[test]
    fn test_export_of_a_larger_tileset_round_trips() {
        let input = temp_path(".mbtiles");
        let output = temp_path(".pmtiles");
        {
            let store = MbtilesStore::create(&input).unwrap();
            store.begin_transaction().unwrap();
            for x in 0..128u32 {
                for y in 0..128u32 {
                    store
                        .put_tile(7, x, y, format!("tile-{}-{}", x, y).as_bytes())
                        .unwrap();
                }
            }
            store.commit_transaction().unwrap();
        }

        let stats = export_mbtiles(&input, &output).unwrap();
        assert_eq!(stats.addressed_tiles, 16384);
        assert_eq!(stats.tile_contents, 16384, "every tile here is distinct");

        let bytes = std::fs::read(&output).unwrap();
        let header = verify_archive(&bytes).unwrap();
        assert!(header.root_length as usize <= MAX_ROOT_DIR_LEN);

        // Corners and interior, through whatever directory layout was chosen
        for (x, y) in [(0u32, 0u32), (63, 100), (127, 127), (12, 3)] {
            assert_eq!(
                read_tile(&bytes, 7, x, y).unwrap().as_deref(),
                Some(format!("tile-{}-{}", x, y).as_bytes()),
                "tile 7/{}/{} did not survive",
                x,
                y
            );
        }

        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
    }

    #[test]
    fn test_export_rejects_an_empty_mbtiles() {
        let input = temp_path(".mbtiles");
        let output = temp_path(".pmtiles");
        {
            MbtilesStore::create(&input).unwrap();
        }

        let err = match export_mbtiles(&input, &output) {
            Err(e) => e,
            Ok(_) => panic!("an empty MBTiles should not produce an archive"),
        };
        assert!(err.to_string().contains("no tiles"), "got: {}", err);

        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn test_verify_archive_catches_a_truncated_file() {
        let input = fixture_mbtiles();
        let output = temp_path(".pmtiles");
        export_mbtiles(&input, &output).unwrap();

        let mut bytes = std::fs::read(&output).unwrap();
        bytes.truncate(bytes.len() / 2);
        assert!(verify_archive(&bytes).is_err());

        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
    }

    // --- metadata ---

    #[test]
    fn test_metadata_flattens_the_mbtiles_json_blob() {
        let mut meta = HashMap::new();
        meta.insert("name".to_string(), "parks".to_string());
        meta.insert(
            "json".to_string(),
            r#"{"vector_layers":[{"id":"parks"}]}"#.to_string(),
        );

        let out: serde_json::Value = serde_json::from_str(&build_metadata(&meta).unwrap()).unwrap();
        assert_eq!(out["name"], "parks");
        assert_eq!(out["vector_layers"][0]["id"], "parks");
        assert!(out.get("json").is_none(), "the raw blob should not survive");
    }

    #[test]
    fn test_metadata_survives_an_unparseable_json_blob() {
        let mut meta = HashMap::new();
        meta.insert("name".to_string(), "parks".to_string());
        meta.insert("json".to_string(), "not json at all".to_string());

        let out: serde_json::Value = serde_json::from_str(&build_metadata(&meta).unwrap()).unwrap();
        assert_eq!(out["name"], "parks");
    }

    #[test]
    fn test_bounds_and_center_parsing() {
        assert_eq!(
            parse_bounds(Some(&"-1.0,50.0,1.0,52.0".to_string())),
            (-1.0, 50.0, 1.0, 52.0)
        );
        assert_eq!(
            parse_center(Some(&"-0.1,51.5,14".to_string())),
            (-0.1, 51.5)
        );
        assert_eq!(
            parse_center_zoom(Some(&"-0.1,51.5,14".to_string())),
            Some(14)
        );

        // Missing or malformed values fall back to the whole world
        let world = parse_bounds(None);
        assert_eq!(world.0, -180.0);
        assert_eq!(world.2, 180.0);
        assert_eq!(parse_bounds(Some(&"garbage".to_string())).0, -180.0);
        assert_eq!(parse_center_zoom(Some(&"-0.1,51.5".to_string())), None);
    }
}
