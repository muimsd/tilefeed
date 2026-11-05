# Derived Geometry Layers

tilefeed can automatically generate companion layers from polygon geometries, useful for map labeling and styling.

## Label Points

Set `generate_label_points = true` on a polygon layer to create a companion point layer named `{name}_labels`, one point per feature with the same id and properties. Choose how the point is made:

| Setting | Point | Use it when |
|---|---|---|
| `label_point_method = "point_on_surface"` (default) | PostGIS `ST_PointOnSurface` | Labels and icons. Always inside the polygon, even for C-shapes, rings and multi-part features. |
| `label_point_method = "centroid"` | PostGIS `ST_Centroid` (center of mass) | Analysis or symbols that should sit at the geometric center. Can fall outside concave or multi-part polygons. |
| `label_point_column = "label_geom"` | An existing point column of the table | You place label points yourself (or maintain them with a trigger). Overrides `label_point_method`. Must be a plain column name. |

Label points are regenerated with the polygons, including incremental updates from LISTEN/NOTIFY. With `label_point_column`, rows are placed by that column (a row whose label column is NULL gets no label), and a point outside its polygon still lands in the tile that contains it. On an update, tiles at the point's new position are refreshed; the trigger's `old_bounds` only covers `geometry_column`, so if you move a label point far outside its polygon, the tile at its old position keeps the stale label until the next full `generate`.

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

Derived layers inherit the same properties as the parent layer. They appear in TileJSON metadata and are included in both full generation and incremental updates.
