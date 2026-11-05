# Derived Geometry Layers

tilefeed can automatically generate companion layers from polygon geometries, useful for map labeling and styling.

## Label Points

Set `generate_label_points = true` on a polygon layer to create a companion point layer named `{name}_labels`. Each point is computed using PostGIS `ST_PointOnSurface`, which guarantees the point lies within the polygon (unlike centroids which can fall outside concave shapes).

```toml
[[sources.layers]]
name = "parks"
table = "parks"
geometry_column = "geom"
id_column = "id"
srid = 4326
properties = ["name", "type"]
generate_label_points = true
# Creates: parks (polygons) + parks_labels (points)
```

## Boundary Lines

Set `generate_boundary_lines = true` to create a companion polyline layer named `{name}_boundary`. Each linestring is computed using PostGIS `ST_Boundary`, giving you the polygon outline as a separate layer for styling borders independently.

```toml
[[sources.layers]]
name = "districts"
table = "districts"
geometry_column = "geom"
id_column = "id"
srid = 4326
properties = ["name"]
generate_boundary_lines = true
# Creates: districts (polygons) + districts_boundary (linestrings)
```

## Both Together

```toml
[[sources.layers]]
name = "parks"
table = "parks"
geometry_column = "geom"
id_column = "id"
srid = 4326
properties = ["name", "type"]
generate_label_points = true
generate_boundary_lines = true
# Creates: parks + parks_labels + parks_boundary
```

Derived layers inherit the same properties as the parent layer, and its [zoom range](configuration.md#per-layer-zoom-range) when `min_zoom`/`max_zoom` is set. They appear in TileJSON metadata and are included in both full generation and incremental updates.
