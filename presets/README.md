# MARS presets

Curated configuration fragments that ship with MARS. Vendor a file into your
own config directory and load it via `!include` from `mars-config`'s YAML.

The config loader restricts `!include` targets to paths under the entry
config's directory (see `crates/support/mars-config/src/include.rs`), so
files in this `presets/` directory have to be **copied** into your config
tree before they can be referenced. They are not loaded directly from the
repo root at runtime.

## Available packs

| File | Contents |
|---|---|
| `symbols.yaml` | Named point-marker `StyleEntry` blocks: filled/hollow circles, squares, triangles, plus cross, X, pin, star, diamond. |

## Usage

Drop the chosen pack under your config directory, then pull it into your
`styles:` block:

```yaml
# my-service/service.yaml
service: { name: demo }
sources: [...]
styles: !include presets/symbols.yaml
layers:
  - name: cities
    source: { id: pg }
    style: { type: ref, name: preset_circle_filled }
```

To mix presets with your own styles, copy individual entries inline rather
than `!include`-ing the whole map - YAML does not splice maps unless you
explicitly merge them, and `mars-config` does not extend the YAML grammar
beyond `!include` plus env-var substitution.

See `docs/symbols.md` for the full marker shape vocabulary and a guide to
authoring custom `vector_shape` symbols.
