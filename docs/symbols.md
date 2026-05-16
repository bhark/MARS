# Marker symbols

Point styles in MARS reference a `marker:` block that describes the shape
to rasterise at each feature anchor. This page documents the shape
vocabulary, how to author your own polygons, and how to consume the stock
preset pack under `presets/`.

## Shape vocabulary

Defined in `crates/domain/mars-style/src/marker.rs` as the `MarkerShape`
enum. The wire form is a flat tagged map (`kind: <shape>`, `size: <px>`,
plus any shape-specific fields).

| `kind`         | Body                                                                              | Notes                                                                       |
|----------------|-----------------------------------------------------------------------------------|-----------------------------------------------------------------------------|
| `circle`       | `size: <px>`                                                                      | Standard filled/stroked circle.                                             |
| `square`       | `size: <px>`                                                                      | Axis-aligned.                                                               |
| `triangle`     | `size: <px>`                                                                      | Upward-pointing isoceles.                                                   |
| `cross`        | `size: <px>`                                                                      | Stroke-only `+` glyph.                                                      |
| `x`            | `size: <px>`                                                                      | Stroke-only diagonal cross.                                                 |
| `pin`          | `size: <px>`                                                                      | Map-pin teardrop. Anchored at the tip.                                      |
| `vector_shape` | `points: [[x, y], ...]`, `anchor: [x, y]`, `filled: bool`, `size: <px>`           | Arbitrary closed polygon in a `[0, 1] x [0, 1]` local frame. See below.     |
| `glyph`        | `font_family: <name>`, `ch: <string>` (alias: `character`), `size: <px>`          | Single text glyph rasterised from a registered font. Default size: 12.      |

`size` is a `ScaledSize`: a bare number (`size: 6.0`) is treated as a fixed
pixel value, while the long form `{ base: 6.0, attenuate: { ... } }` lets
the value scale with the active denom band. See `mars-style::ScaledSize`.

Fill, stroke, and stroke-width come from the enclosing point `Style`:

```yaml
my_marker:
  type: point
  fill: "#000000"          # omit for a hollow marker
  stroke: "#ffffff"
  stroke_width: 1.0
  marker: { kind: circle, size: 6.0 }
```

The `cross` / `x` shapes are stroke-only by design; `fill` is ignored.

## Authoring a `vector_shape`

`vector_shape` accepts any closed polygon described in a unit-square local
frame. The renderer transforms each vertex via
`(x - anchor.0) * size, (y - anchor.1) * size`, so the polygon is scaled
uniformly by `size` and translated so `anchor` lands at the feature
position.

Conventions:

- The local frame is `[0, 1] x [0, 1]`.
- The y-axis is screen-down (y = 0 is the top edge); a triangle pointing
  visually up has its apex at `(0.5, 0.0)`.
- `anchor: [0.5, 0.5]` is the centre. Use a different anchor for shapes
  that should hang from a specific feature point, e.g. a pin tip at
  `anchor: [0.5, 1.0]`.
- `filled: true` closes the subpath and routes through the fill pipeline;
  `filled: false` emits an open polyline (the dispatch hub clears `fill`
  in this case so the stroke path runs cleanly).
- Winding order does not matter for filling: the renderer uses the
  non-zero winding rule.

Worked example - a five-pointed star with outer radius 0.5 and inner
radius 0.2, centred at `(0.5, 0.5)`:

```yaml
my_star:
  type: point
  fill: "#000000"
  marker:
    kind: vector_shape
    points:
      - [0.500, 0.000]   # top apex
      - [0.618, 0.338]
      - [0.976, 0.345]   # upper-right arm
      - [0.690, 0.562]
      - [0.794, 0.905]   # lower-right arm
      - [0.500, 0.700]
      - [0.206, 0.905]   # lower-left arm
      - [0.310, 0.562]
      - [0.024, 0.345]   # upper-left arm
      - [0.382, 0.338]
    anchor: [0.5, 0.5]
    filled: true
    size: 10.0
```

## Using the stock preset pack

The repo ships a curated pack of named point styles under
`presets/symbols.yaml`:

```
preset_circle_filled       preset_circle_hollow
preset_square_filled       preset_square_hollow
preset_triangle_filled     preset_triangle_hollow
preset_cross               preset_x
preset_pin
preset_diamond_filled      preset_diamond_hollow
preset_star_filled         preset_star_hollow
```

The pack is monochrome on purpose - copy any entry into your own
`styles:` block and adjust `fill` / `stroke` / `stroke_width` for your
palette. To pull the whole pack in via `!include`, vendor the file into
your config directory (the loader rejects includes outside the entry
config's root, see `crates/support/mars-config/src/include.rs`) and:

```yaml
styles: !include presets/symbols.yaml
layers:
  - name: cities
    source: { id: pg }
    style: { type: ref, name: preset_circle_filled }
```

Mixing presets with bespoke styles is a copy-paste exercise today; MARS
does not splice maps across `!include` boundaries. If you want a deeper
reuse mechanism, file an issue.

## Glyph markers and fonts

`kind: glyph` rasterises a single text glyph from a registered font.
Glyph markers route through the same font registry as labels; see
[`fonts.md`](fonts.md) for the discovery and deployment story.
