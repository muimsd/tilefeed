# PMTiles Export

[PMTiles](https://github.com/protomaps/PMTiles) packs an entire tileset into a single
file that clients read with HTTP range requests. Put it on S3, R2, or any static host
and a map works with no tile server, no SQLite, and no running process.

```bash
tilefeed export basemap.mbtiles basemap.pmtiles
```

```
PMTiles archive: basemap.pmtiles

  Zoom levels:       0-14
  Tiles:             41532
  Directory entries: 38104 (3428 saved by runs)
  Distinct blobs:    31905 (6199 deduplicated)
  Leaf directories:  4
  Size:              184203841 bytes
  Verified:          64 tiles re-read and compared against the source
```

The command reads an MBTiles file directly, so it needs neither the database nor the
config — it composes with `inspect` and `diff` the same way. It refuses to write over
its own input.

## Serving it

Upload the file and point a client at it. Nothing else is required:

```bash
aws s3 cp basemap.pmtiles s3://my-bucket/ --content-type application/octet-stream
```

```html
<script src="https://unpkg.com/pmtiles@3/dist/pmtiles.js"></script>
<script>
  const protocol = new pmtiles.Protocol();
  maplibregl.addProtocol("pmtiles", protocol.tile);

  new maplibregl.Map({
    container: "map",
    style: {
      version: 8,
      sources: {
        basemap: {
          type: "vector",
          url: "pmtiles://https://my-bucket.s3.amazonaws.com/basemap.pmtiles",
        },
      },
      layers: [/* ... */],
    },
  });
</script>
```

The bucket must allow range requests (S3 and R2 do by default) and CORS if the page is
on another origin.

## What the export does

**Deduplicates.** Tiles with identical bytes are stored once. Ocean, empty land, and
unchanged areas at low zoom collapse hard — the `Distinct blobs` line reports how many
were saved.

**Collapses runs.** Consecutive tile IDs sharing one blob become a single directory
entry with a run length, rather than one entry each.

**Orders tiles along a Hilbert curve.** Tiles that are near each other on the map end
up near each other in the file, so a client panning around tends to hit bytes it has
already fetched.

**Builds leaf directories when needed.** The header and root directory have to fit in
16 KiB so a client can fetch both in one request. Small tilesets need only a root; past
that, entries are split into leaves and the root indexes them. `Leaf directories: 0`
means everything fit in the root.

## Format notes

The writer implements [PMTiles v3](https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md):
a 127-byte header, then the root directory, JSON metadata, leaf directories, and tile
data. Directories are stored as varint columns and gzipped.

- **Tile type** is always MVT.
- **Tile compression is always gzip.** One MBTiles can hold both raw and gzipped
  tiles — Tippecanoe with `no_tile_compression` writes raw, while incremental
  updates always gzip — and PMTiles has a single header field describing all of
  them. Raw tiles are compressed on the way in so the header is never a lie.
- **Metadata** comes from the MBTiles `metadata` table. The `json` blob that Tippecanoe
  writes is flattened into the top level, so `vector_layers` lands where clients expect.
- **Bounds and center** come from the MBTiles metadata, falling back to the whole world.
- Archives are marked **clustered**, since tile data is written in tile-ID order.

Every archive is read back before `export` reports success. Two levels of check:

- **Structural** — the header parses, directories parse, and every entry points
  inside the tile data section.
- **Against the source** — up to 64 tiles spread across the archive are re-read and
  compared byte-for-byte with the MBTiles they came from. A structural check alone
  cannot catch a tile written at the wrong coordinate: such an archive is perfectly
  self-consistent and completely wrong.

The `Verified:` line in the output reports how many tiles were compared.

## Verifying an archive

The writer was checked against the reference implementation by hand during
development — the Python `pmtiles` package reads its archives, and every tile ID
matches `zxy_to_tileid` across z0–z15. The repository's own tests do not depend on
that package, so you can repeat the check yourself:

```bash
pip install pmtiles
pmtiles-show basemap.pmtiles          # header and metadata
pmtiles-show basemap.pmtiles 14 8188 5448   # one tile
```

## Limits

- Tile blobs are streamed through a staging file next to the output, so memory scales
  with the *number* of tiles (a directory entry each) rather than their size. Expect
  roughly 40 bytes per tile plus the tile currently being processed — a 164 MB
  archive of 40,000 tiles peaks at about 28 MB.
- The source is opened read-only, so exporting from read-only media or an immutable
  artifact works and never leaves `-wal`/`-shm` files behind.
- One level of leaf directories, which the spec recommends and which covers tilesets
  into the tens of millions of tiles.
- Export is a point-in-time snapshot: incremental updates land in MBTiles, so re-export
  after an update to refresh a published archive.
