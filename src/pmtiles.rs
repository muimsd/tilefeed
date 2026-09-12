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
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
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
///
/// Entries must be sorted by ascending `tile_id`: the IDs are delta-encoded, so
/// an unsorted slice underflows into a directory that still parses and resolves
/// nothing.
pub fn serialize_directory(entries: &[Entry]) -> Vec<u8> {
    debug_assert!(
        entries.windows(2).all(|w| w[0].tile_id <= w[1].tile_id),
        "directory entries must be sorted by ascending tile_id"
    );

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

    // Grow the leaf size until the root fits: bigger leaves mean fewer of them,
    // and the root holds one run_length=0 pointer per leaf.
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
    /// Tiles re-read from the finished archive and compared against the source
    pub tiles_verified: usize,
}

/// Reads an archive through any seekable source — a file, or a `Cursor` in tests.
///
/// Every read is bounds-checked by the reader itself, so a truncated or
/// malformed archive produces an error rather than a panic, and nothing loads
/// the whole archive into memory.
pub struct ArchiveReader<R: Read + Seek> {
    inner: R,
    header: Header,
    len: u64,
}

impl<R: Read + Seek> ArchiveReader<R> {
    pub fn open(mut inner: R) -> Result<Self> {
        let len = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(0))?;

        let mut buf = [0u8; HEADER_LEN];
        inner
            .read_exact(&mut buf)
            .context("archive is shorter than a PMTiles header")?;
        let header = Header::from_bytes(&buf)?;

        Ok(Self { inner, header, len })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    fn read_at(&mut self, offset: u64, length: usize) -> Result<Vec<u8>> {
        // Offsets come from the file being read, so they can be anything at all
        let end = offset
            .checked_add(length as u64)
            .context("archive section offset overflows")?;
        if end > self.len {
            bail!(
                "archive is {} bytes but a section at {}+{} was requested",
                self.len,
                offset,
                length
            );
        }
        self.inner.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; length];
        self.inner.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn directory(&mut self, offset: u64, length: u64) -> Result<Vec<Entry>> {
        let raw = self.read_at(offset, length as usize)?;
        deserialize_directory(&ungzip(&raw)?)
    }

    fn root(&mut self) -> Result<Vec<Entry>> {
        self.directory(self.header.root_offset, self.header.root_length)
    }

    /// Read one tile, following a leaf directory when the root points at one.
    pub fn get_tile(&mut self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>> {
        let wanted = tile_id(z, x, y);
        let mut entries = self.root()?;

        // At most one level of leaves, as the spec recommends
        for _ in 0..2 {
            let entry = match entries.iter().rev().find(|e| e.tile_id <= wanted).cloned() {
                Some(entry) => entry,
                None => return Ok(None),
            };

            if entry.run_length == 0 {
                let offset = self
                    .header
                    .leaf_offset
                    .checked_add(entry.offset)
                    .context("leaf directory offset overflows")?;
                entries = self.directory(offset, entry.length as u64)?;
                continue;
            }

            if wanted >= entry.tile_id.saturating_add(entry.run_length as u64) {
                return Ok(None);
            }
            let offset = self
                .header
                .data_offset
                .checked_add(entry.offset)
                .context("tile offset overflows")?;
            return Ok(Some(self.read_at(offset, entry.length as usize)?));
        }

        Ok(None)
    }

    /// Check the archive describes itself consistently: sections are inside the
    /// file, directories parse, and every entry points inside the tile data.
    ///
    /// This is a structural check only. It cannot tell whether tiles ended up at
    /// the right coordinates, which is why [`export_mbtiles`] also compares
    /// sampled tiles against the source.
    pub fn verify(&mut self) -> Result<()> {
        let declared_end = self
            .header
            .data_offset
            .checked_add(self.header.data_length)
            .context("header describes a tile data section that overflows")?;
        if declared_end > self.len {
            bail!(
                "archive is {} bytes but the header describes {}",
                self.len,
                declared_end
            );
        }

        let data_length = self.header.data_length;
        let leaf_offset = self.header.leaf_offset;

        fn check(entry: &Entry, data_length: u64) -> Result<()> {
            let end = entry
                .offset
                .checked_add(entry.length as u64)
                .context("directory entry offset overflows")?;
            if end > data_length {
                bail!(
                    "tile {} points outside the tile data section",
                    entry.tile_id
                );
            }
            Ok(())
        }

        let root = self.root()?;
        let mut tile_entries = 0u64;
        for entry in &root {
            if entry.run_length == 0 {
                let offset = leaf_offset
                    .checked_add(entry.offset)
                    .context("leaf directory offset overflows")?;
                let leaf = self.directory(offset, entry.length as u64)?;
                for leaf_entry in &leaf {
                    check(leaf_entry, data_length)?;
                }
                tile_entries += leaf.len() as u64;
            } else {
                check(entry, data_length)?;
                tile_entries += 1;
            }
        }

        if tile_entries != self.header.tile_entries {
            bail!(
                "header says {} tile entries, directories hold {}",
                self.header.tile_entries,
                tile_entries
            );
        }
        Ok(())
    }
}

fn ungzip(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    let mut out = Vec::new();
    GzDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

/// Reject an output path that is the input, or that already exists as the input
/// under another spelling. Export reads every tile before it writes, so writing
/// over the source destroys it and leaves an archive that verifies against
/// itself perfectly.
fn check_paths(input: &str, output: &str) -> Result<()> {
    let input_path = std::path::Path::new(input);
    let output_path = std::path::Path::new(output);

    if input_path == output_path {
        bail!("input and output are the same file: {}", input);
    }

    // Catches ./a.mbtiles vs a.mbtiles, symlinks, and case-insensitive filesystems
    if let (Ok(a), Ok(b)) = (input_path.canonicalize(), output_path.canonicalize()) {
        if a == b {
            bail!("output {} is the same file as the input {}", output, input);
        }
    }
    Ok(())
}

/// Convert an MBTiles file into a PMTiles archive.
///
/// Tile blobs are staged in a temporary file next to the output, because the
/// directories that precede them can only be written once every offset is known.
/// Memory stays proportional to the number of tiles, not to their size.
pub fn export_mbtiles(input: &str, output: &str) -> Result<ExportStats> {
    check_paths(input, output)?;

    let store = MbtilesStore::open_read_only(input)?;
    let metadata: HashMap<String, String> = store.get_all_metadata()?.into_iter().collect();

    let staging_path = format!("{}.tiles-staging", output);
    let staging = std::fs::File::create(&staging_path)
        .with_context(|| format!("Failed to create staging file {}", staging_path))?;
    // Whatever happens next, don't leave the staging file behind
    let _cleanup = StagingFile(staging_path.clone());
    let mut staging = BufWriter::new(staging);

    let mut entries: Vec<Entry> = Vec::new();
    let mut seen: HashMap<[u8; 32], (u64, u32)> = HashMap::new();
    let mut addressed_tiles: u64 = 0;
    let mut data_len: u64 = 0;
    let mut min_zoom = u8::MAX;
    let mut max_zoom = 0u8;
    // Coordinates only. Tiles have to be written in Hilbert order, which means
    // reordering them, and holding every tile's bytes to do that would put the
    // whole archive in memory. 16 bytes per tile instead.
    let mut ordered: Vec<(u64, u8, u32, u32)> = Vec::new();
    store.for_each_tile_coord(|z, x, tms_y| {
        let y = flip_y(z, x, tms_y)?;
        min_zoom = min_zoom.min(z);
        max_zoom = max_zoom.max(z);
        ordered.push((tile_id(z, x, y), z, x, tms_y));
        Ok(())
    })?;

    if ordered.is_empty() {
        bail!("{} contains no tiles", input);
    }

    // Hilbert order: what makes the archive clustered, and what lets a client
    // panning the map reuse ranges it has already fetched.
    ordered.sort_by_key(|(id, _, _, _)| *id);

    for (id, z, x, tms_y) in &ordered {
        let data = match store.get_tile_raw_tms(*z, *x, *tms_y)? {
            Some(data) => data,
            None => continue, // removed since the coordinates were listed
        };
        addressed_tiles += 1;

        // Archives are uniformly gzipped. A tilefeed MBTiles can hold both raw
        // and gzipped tiles — Tippecanoe with `no_tile_compression` writes raw,
        // while incremental updates always gzip — and a single header field has
        // to describe all of them.
        let blob = if crate::mvt::is_gzipped(&data) {
            data
        } else {
            gzip(&data)?
        };

        let hash: [u8; 32] = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&blob);
            hasher.finalize().into()
        };

        // A run: the tile immediately before this one, with the same content
        if let Some(last) = entries.last_mut() {
            if last.tile_id + last.run_length as u64 == *id
                && seen.get(&hash) == Some(&(last.offset, last.length))
            {
                last.run_length += 1;
                continue;
            }
        }

        match seen.get(&hash) {
            Some(&(offset, length)) => entries.push(Entry {
                tile_id: *id,
                offset,
                length,
                run_length: 1,
            }),
            None => {
                let offset = data_len;
                let length = blob.len() as u32;
                staging.write_all(&blob)?;
                data_len += length as u64;
                seen.insert(hash, (offset, length));
                entries.push(Entry {
                    tile_id: *id,
                    offset,
                    length,
                    run_length: 1,
                });
            }
        }
    }
    drop(ordered);
    staging.flush()?;
    drop(staging);

    let (root, leaves, leaf_count) = build_directories(&entries)?;
    let metadata_bytes = gzip(build_metadata(&metadata)?.as_bytes())?;

    let root_offset = HEADER_LEN as u64;
    let metadata_offset = root_offset + root.len() as u64;
    let leaf_offset = metadata_offset + metadata_bytes.len() as u64;
    let data_offset = leaf_offset + leaves.len() as u64;

    let bounds = parse_bounds(metadata.get("bounds"));
    let header = Header {
        root_offset,
        root_length: root.len() as u64,
        metadata_offset,
        metadata_length: metadata_bytes.len() as u64,
        leaf_offset,
        leaf_length: leaves.len() as u64,
        data_offset,
        data_length: data_len,
        addressed_tiles,
        tile_entries: entries.len() as u64,
        tile_contents: seen.len() as u64,
        clustered: true,
        internal_compression: Compressed::Gzip,
        tile_compression: Compressed::Gzip,
        tile_type: TileType::Mvt,
        min_zoom,
        max_zoom,
        bounds,
        center_zoom: parse_center_zoom(metadata.get("center")).unwrap_or(min_zoom),
        // Null Island is a worse default than the middle of the data
        center: parse_center(metadata.get("center"))
            .unwrap_or(((bounds.0 + bounds.2) / 2.0, (bounds.1 + bounds.3) / 2.0)),
    };

    {
        let out = std::fs::File::create(output)
            .with_context(|| format!("Failed to create {}", output))?;
        let mut out = BufWriter::new(out);
        out.write_all(&header.to_bytes())?;
        out.write_all(&root)?;
        out.write_all(&metadata_bytes)?;
        out.write_all(&leaves)?;

        let mut staged = std::fs::File::open(&staging_path)?;
        std::io::copy(&mut staged, &mut out)?;
        out.flush()?;
    }

    let bytes_written = data_offset + data_len;
    let tiles_verified = verify_against_source(output, &store, &sample_tile_ids(&entries))?;

    Ok(ExportStats {
        addressed_tiles,
        tile_entries: entries.len() as u64,
        tile_contents: seen.len() as u64,
        leaf_directories: leaf_count,
        bytes_written,
        min_zoom,
        max_zoom,
        tiles_verified,
    })
}

/// Removes the staging file on the way out, however we leave.
struct StagingFile(String);

impl Drop for StagingFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// MBTiles rows are TMS, PMTiles tile IDs are XYZ. A row outside the zoom's
/// range would wrap into a nonsense coordinate, so reject it by name.
fn flip_y(z: u8, x: u32, tms_y: u32) -> Result<u32> {
    if z > 30 {
        bail!("zoom {} is out of range (0-30)", z);
    }
    let side = 1u32 << z;
    if x >= side || tms_y >= side {
        bail!(
            "tile {}/{}/{} is outside the {}x{} grid of zoom {}",
            z,
            x,
            tms_y,
            side,
            side,
            z
        );
    }
    Ok(side - 1 - tms_y)
}

/// Spread a sample across the archive rather than taking the first N, so a
/// coordinate-mapping error anywhere shows up.
fn sample_tile_ids(entries: &[Entry]) -> Vec<u64> {
    const SAMPLE: usize = 64;
    if entries.len() <= SAMPLE {
        return entries.iter().map(|e| e.tile_id).collect();
    }
    let step = entries.len() / SAMPLE;
    (0..SAMPLE).map(|i| entries[i * step].tile_id).collect()
}

/// Re-read sampled tiles from the finished archive and compare them with the
/// source. A structural check cannot catch a tile written at the wrong
/// coordinate — the archive would be perfectly self-consistent and wrong.
fn verify_against_source(output: &str, store: &MbtilesStore, sample: &[u64]) -> Result<usize> {
    let file = std::fs::File::open(output)?;
    let mut reader = ArchiveReader::open(file)?;
    reader.verify()?;

    let mut verified = 0;
    for id in sample {
        let (z, x, y) = tile_id_to_zxy(*id)?;
        let tms_y = (1u32 << z) - 1 - y;

        let expected = store
            .get_tile_raw_tms(z, x, tms_y)?
            .with_context(|| format!("tile {}/{}/{} vanished from the source", z, x, y))?;
        let expected = if crate::mvt::is_gzipped(&expected) {
            expected
        } else {
            gzip(&expected)?
        };

        let actual = reader
            .get_tile(z, x, y)?
            .with_context(|| format!("tile {}/{}/{} is missing from the archive", z, x, y))?;

        if actual != expected {
            bail!(
                "tile {}/{}/{} does not match the source: the archive is mis-addressed",
                z,
                x,
                y
            );
        }
        verified += 1;
    }
    Ok(verified)
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

/// `None` when the MBTiles has no usable `center`, so the caller can fall back
/// to the middle of the bounds rather than to Null Island.
fn parse_center(raw: Option<&String>) -> Option<(f64, f64)> {
    let parts: Vec<f64> = raw?
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    match parts[..] {
        [lon, lat] | [lon, lat, _] => Some((lon, lat)),
        _ => None,
    }
}

fn parse_center_zoom(raw: Option<&String>) -> Option<u8> {
    let parts: Vec<&str> = raw?.split(',').collect();
    parts.get(2)?.trim().parse::<f64>().ok().map(|z| z as u8)
}

/// CLI entry point for `tilefeed export`.
pub fn export(input: &str, output: &str) -> Result<()> {
    let stats = export_mbtiles(input, output)?;

    info!("Wrote {} ({} bytes)", output, stats.bytes_written);
    println!("PMTiles archive: {}", output);
    println!();
    println!("  Zoom levels:       {}-{}", stats.min_zoom, stats.max_zoom);
    println!("  Tiles:             {}", stats.addressed_tiles);
    println!(
        "  Directory entries: {} ({} saved by runs)",
        stats.tile_entries,
        stats.addressed_tiles - stats.tile_entries
    );
    println!(
        "  Distinct blobs:    {} ({} deduplicated)",
        stats.tile_contents,
        stats.tile_entries - stats.tile_contents
    );
    println!("  Leaf directories:  {}", stats.leaf_directories);
    println!("  Size:              {} bytes", stats.bytes_written);
    println!(
        "  Verified:          {} tiles re-read and compared against the source",
        stats.tiles_verified
    );

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
    fn test_zoom_ranges_are_adjacent_with_no_gap_or_overlap() {
        // Ask tile_id itself: the highest id at one zoom must be exactly one
        // below the lowest at the next, or ids from different zooms collide.
        for z in 0u8..12 {
            let side = 1u32 << z;
            let highest = (0..side)
                .flat_map(|x| (0..side).map(move |y| (x, y)))
                .map(|(x, y)| tile_id(z, x, y))
                .max()
                .unwrap();
            assert_eq!(
                highest + 1,
                tile_id(z + 1, 0, 0),
                "zoom {} and {} are not adjacent",
                z,
                z + 1
            );
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

    fn open_archive(path: &str) -> ArchiveReader<std::fs::File> {
        ArchiveReader::open(std::fs::File::open(path).unwrap()).unwrap()
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

        let mut archive = open_archive(&output);
        archive.verify().unwrap();

        for (z, x, y, expected) in [
            (0u8, 0u32, 0u32, &b"tile-zero"[..]),
            (1, 0, 0, &b"tile-a"[..]),
            (1, 1, 0, &b"tile-b"[..]),
            (1, 1, 1, &b"tile-a"[..]),
            (2, 1, 1, &b"tile-c"[..]),
        ] {
            assert_eq!(
                archive.get_tile(z, x, y).unwrap().as_deref(),
                Some(&gzip(expected).unwrap()[..]),
                "tile {}/{}/{} did not survive the round trip",
                z,
                x,
                y
            );
        }

        // A tile that was never written
        assert_eq!(archive.get_tile(2, 0, 0).unwrap(), None);

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

        let mut archive = open_archive(&output);
        let expected = gzip(b"identical").unwrap();
        for x in 0..4 {
            for y in 0..4 {
                assert_eq!(
                    archive.get_tile(2, x, y).unwrap().as_deref(),
                    Some(&expected[..])
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
        // Archives are uniformly gzipped, whatever the source held
        assert_eq!(header.tile_compression, Compressed::Gzip);

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

        let mut archive = open_archive(&output);
        archive.verify().unwrap();
        assert!(archive.header().root_length as usize <= MAX_ROOT_DIR_LEN);

        // Corners and interior, through whatever directory layout was chosen
        for (x, y) in [(0u32, 0u32), (63, 100), (127, 127), (12, 3)] {
            let expected = gzip(format!("tile-{}-{}", x, y).as_bytes()).unwrap();
            assert_eq!(
                archive.get_tile(7, x, y).unwrap().as_deref(),
                Some(&expected[..]),
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
    fn test_verify_catches_a_truncated_archive() {
        let input = fixture_mbtiles();
        let output = temp_path(".pmtiles");
        export_mbtiles(&input, &output).unwrap();

        let mut bytes = std::fs::read(&output).unwrap();
        bytes.truncate(bytes.len() / 2);

        let mut archive = ArchiveReader::open(std::io::Cursor::new(bytes)).unwrap();
        assert!(archive.verify().is_err());

        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
    }

    #[test]
    fn test_reader_errors_rather_than_panicking_on_a_bogus_header() {
        // Header claims sections far beyond the end of the file
        let mut bytes = sample_header().to_bytes().to_vec();
        bytes[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        let mut archive = ArchiveReader::open(std::io::Cursor::new(bytes)).unwrap();
        assert!(archive.verify().is_err());
        assert!(archive.get_tile(0, 0, 0).is_err());
    }

    #[test]
    fn test_export_refuses_to_overwrite_its_input() {
        // Tiles are read before anything is written, so writing over the source
        // would destroy it and leave an archive that verifies against itself.
        let input = fixture_mbtiles();
        let err = match export_mbtiles(&input, &input) {
            Err(e) => e,
            Ok(_) => panic!("exporting onto the input must fail"),
        };
        assert!(err.to_string().contains("same file"), "got: {}", err);

        // ...and the source is untouched
        let store = MbtilesStore::open(&input).unwrap();
        assert_eq!(store.tile_count().unwrap(), 5);

        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn test_export_normalizes_mixed_compression() {
        // Tippecanoe with `no_tile_compression` writes raw tiles while the
        // incremental updater always gzips, so one MBTiles can hold both. A
        // single header field has to describe every tile.
        let input = temp_path(".mbtiles");
        let output = temp_path(".pmtiles");
        {
            let store = MbtilesStore::create(&input).unwrap();
            store.put_tile(0, 0, 0, b"raw tile bytes").unwrap();
            store
                .put_tile(1, 0, 0, &gzip(b"gzipped tile bytes").unwrap())
                .unwrap();
        }

        export_mbtiles(&input, &output).unwrap();
        let mut archive = open_archive(&output);
        assert_eq!(archive.header().tile_compression, Compressed::Gzip);

        for (z, x, y, expected) in [
            (0u8, 0u32, 0u32, &b"raw tile bytes"[..]),
            (1, 0, 0, &b"gzipped tile bytes"[..]),
        ] {
            let stored = archive.get_tile(z, x, y).unwrap().unwrap();
            assert!(
                crate::mvt::is_gzipped(&stored),
                "every tile must be gzipped"
            );
            assert_eq!(ungzip(&stored).unwrap(), expected);
        }

        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
    }

    #[test]
    fn test_export_rejects_a_tile_row_outside_its_zoom() {
        // A foreign or corrupt MBTiles must produce an error, not a panic in
        // debug or a silently mis-addressed archive in release.
        let input = temp_path(".mbtiles");
        let output = temp_path(".pmtiles");
        {
            let store = MbtilesStore::create(&input).unwrap();
            store.put_tile(1, 0, 0, b"fine").unwrap();
        }
        {
            // Row 9 does not exist at zoom 1. Written directly, because no
            // production path can produce it — only a foreign or corrupt file.
            let conn = rusqlite::Connection::open(&input).unwrap();
            conn.execute(
                "INSERT INTO tiles (zoom_level, tile_column, tile_row, tile_data)
                 VALUES (1, 0, 9, ?1)",
                [&b"impossible"[..]],
            )
            .unwrap();
        }

        let err = match export_mbtiles(&input, &output) {
            Err(e) => e,
            Ok(_) => panic!("an out-of-range tile row must be rejected"),
        };
        assert!(err.to_string().contains("outside"), "got: {}", err);

        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
    }

    #[test]
    fn test_center_falls_back_to_the_middle_of_the_bounds() {
        let input = temp_path(".mbtiles");
        let output = temp_path(".pmtiles");
        {
            let store = MbtilesStore::create(&input).unwrap();
            store
                .set_metadata("bounds", "-10.0,40.0,10.0,50.0")
                .unwrap();
            store.put_tile(0, 0, 0, b"tile").unwrap();
        }

        export_mbtiles(&input, &output).unwrap();
        let header = open_archive(&output).header().clone();
        // Not (0,0): Null Island would send a viewer to the Atlantic
        assert!((header.center.0 - 0.0).abs() < 1e-7);
        assert!((header.center.1 - 45.0).abs() < 1e-7);

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
            Some((-0.1, 51.5))
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
        // No center at all: the caller supplies the bounds midpoint instead
        assert_eq!(parse_center(None), None);
        assert_eq!(parse_center(Some(&"garbage".to_string())), None);
    }
}
