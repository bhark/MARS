# Fonts

MARS rasterises labels and `glyph` markers from TrueType / OpenType faces
loaded at runtime startup. This page covers where those faces come from,
how the mapfile importer resolves `FONT <alias>` references, and how to
mount custom fonts when the runtime is deployed via the operator.

## Runtime discovery

The font registry lives in the `mars-text` crate
(`crates/support/mars-text/src/lib.rs`). `Fonts::load(paths, bundle_default)`
walks each path in `paths` as a directory tree, registers every `.ttf` /
`.otf` it finds, and (when `bundle_default` is `true`) appends a vendored
DejaVu Sans last so the fallback path is reachable even when no system
fonts are present.

The service config knob is `service.fonts` in `mars-config`:

```yaml
service:
  name: demo
  fonts:
    paths:
      - /var/lib/mars/fonts            # walked recursively
    bundle_default: true               # vendored DejaVu Sans as fallback
```

Both fields are optional; the defaults are `paths: []` and
`bundle_default: true`. Missing directories are logged as warnings, not
fatal errors, so a typo in `paths` results in blank labels rather than a
startup crash - check the runtime startup logs (`font path skipped`) if
labels render with the default face only.

Resolution order at render time:

1. The label or marker's authored `font_family`.
2. `DejaVu Sans` (the vendored fallback when `bundle_default: true`).
3. `Family::SansSerif` (whatever fontdb settles on, typically the first
   registered face).

## Mapfile `FONT` and `FONTSET` resolution

MapServer mapfiles reference fonts through `FONTSET "fontset.txt"` at MAP
scope, where `fontset.txt` is a flat `<alias> <filename>` table. The
mapfile importer (`bin/mars-import-mapfile`) parses the fontset and
rewrites every `LABEL.FONT "<alias>"` and `SYMBOL TYPE TRUETYPE FONT
"<alias>"` into the actual font-family name picked up from the file's
metadata. Implementation:
`bin/mars-import-mapfile/src/translate/fontset.rs`.

Effect:

- The emitted MARS YAML never contains MapServer-style aliases; every
  `font_family` is the resolved family name as `fontdb` sees it.
- Unresolved aliases (file missing, parse error, alias not declared) are
  passed through verbatim with a warning. The runtime will then fall
  back to the bundled face at render time.
- The fontset itself is **not** packaged into the MARS artifact - it is
  only consulted during import. At runtime, your `service.fonts.paths`
  must independently include the directories holding those font files.

## Deploying custom fonts via the operator

The operator can mount any Kubernetes volume into the runtime pod via
`spec.runtime.extraVolumes` and `spec.runtime.extraVolumeMounts` on the
`MarsService` CR. Point your `service.fonts.paths` at the mount path and
the runtime picks the faces up at startup.

Example - mounting a Secret holding two TTF files:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: custom-fonts
type: Opaque
data:
  Roboto-Regular.ttf: <base64-payload>
  Roboto-Bold.ttf:    <base64-payload>
---
apiVersion: mars.forn.dk/v1alpha1
kind: MarsService
metadata:
  name: demo
spec:
  compiler: {}
  runtime:
    replicas: 2
    extraVolumes:
      - name: fonts
        secret:
          secretName: custom-fonts
    extraVolumeMounts:
      - name: fonts
        mountPath: /var/lib/mars/fonts
        readOnly: true
  config:
    service:
      name: demo
      fonts:
        paths: ["/var/lib/mars/fonts"]
        bundle_default: true
    sources: [...]
    layers: [...]
```

A ConfigMap (`configMap.name`) works the same way for small font sets;
for large fontsets or rotation across services, mount a shared PVC.

Reserved volume names on the runtime pod are `config`, `cache`, and
`artifact-store`. The operator does not detect collisions today - if you
pick one of those names for `extraVolumes`, the apiserver will reject the
resulting PodSpec at admission.

## Verification

After deploying, check that the runtime registered your faces:

```sh
kubectl logs deploy/<service>-runtime | grep -i font
```

You should see one `font path skipped` warning per missing directory and
no warnings for the directory you mounted. Render a tile that uses a
label styled with one of the custom families - if it falls back to
DejaVu, recheck the mount path, the `service.fonts.paths` value, and
that the family name in your YAML matches the face's actual family-name
field (read with `fc-query <file>` or `ttx -t name <file>`).
