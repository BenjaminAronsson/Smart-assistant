# jarvis-app-builder

Out-of-process **app builder** for Jarvis — F6.2, FR-18, `docs/06` §6, ADR-027, ADR-029.

Renders a **host-validated app spec** against a **locked Vite template** and returns
**one self-contained HTML document** on stdout. It decides nothing: the Rust host
(`jarvis-adapters::app_builder`) owns the `ToolPolicy`, the size/time caps, the build
provenance and the artifact write.

## Protocol

Line-delimited JSON over stdio, one exchange per line.

```jsonc
// host → worker
{"build_id": 1, "template": "dashboard/v1", "title": "Kitchen",
 "capabilities": ["home.read_state"],
 "bindings": [{"name": "kitchen_temp", "capability": "home.read_state",
               "target": "sensor.kitchen_temperature"}],
 "max_bundle_bytes": 2097152, "max_build_seconds": 120}

// worker → host
{"ok": true, "bundle": "<!doctype html>…", "summary": "built dashboard/v1", "error": null}
```

The host reads only those four reply fields. There is deliberately **no field for build
provenance**: the worker must not be able to declare its own isolation posture — the host
attests the worker image, the lockfile hash and the network posture of the profile it
actually launched (`docs/06` §5/§6).

## Templates

`templates/<name>/` holds a locked template: its `package.json`, its **committed
`package-lock.json`**, its Vite config and its source. A spec *selects* a template by id
(`dashboard/v1`); it never supplies code. The id → directory map lives in `src/index.mjs`
and mirrors `jarvis_domain::appspec::TemplateId`, so a template id can never be used as a
path component.

`dashboard/v1` builds to a single file (`vite-plugin-singlefile`). That is a security
property, not packaging taste: one document means no archive to extract, no path
traversal, no zip-slip and nothing for F6.4's sandboxed origin to serve but the document
itself. The host **refuses** a bundle containing an external subresource, and F6.4's CSP
forbids fetching one at render time.

## Dependencies

A build never resolves a dependency, so a build never needs the network.

* **Production:** the worker image carries `node_modules`, installed from the committed
  lockfile at image build time. Per **ADR-027**, the container is the contract.
* **Dev/CI:** install once, then builds are offline:

  ```bash
  npm --prefix tools/app-builder run install-templates   # npm ci in each template
  ```

  The host records `network: enabled` in this profile, because that is what is true —
  see **D-M6-1**. `network: disabled` is reserved for a launch profile that actually
  isolates the network; the host refuses to attest it otherwise.

A missing install is reported as `template dependencies are not installed (npm ci)`
rather than surfacing as a generic build failure.

## Tests

The host's Rust tests drive a **fake** transport, so CI needs neither Node nor Vite —
the same discipline as `tools/coding-worker` and `tools/browser-worker`. One fixture,
`crates/jarvis-adapters/tests/fixtures/dashboard-v1-built.html`, is verbatim output of a
**real** build and is asserted to pass the host's static checks; regenerate it with:

```bash
echo '{"build_id":1,"template":"dashboard/v1","title":"Kitchen","capabilities":[],"bindings":[],"max_bundle_bytes":2097152,"max_build_seconds":120}' \
  | node tools/app-builder/src/index.mjs
```
