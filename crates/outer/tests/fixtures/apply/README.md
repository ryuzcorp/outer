# Apply golden fixtures

One JSON case per file. Both apply engines (Rust `crates/outer/src/examples/apply.rs`,
TypeScript `scripts/apply-example.ts` in outer-templates) run these inputs and must
produce byte-identical outputs.

## Format

```json
{
  "template": "app",
  "tree": { "path": "full text\n", "...": "..." },
  "examples": [
    {
      "name": "photos",
      "template": "app",
      "requires": ["passkeys"],
      "nav": [{ "href": "/photos", "label": "Photos", "icon": "Camera" }],
      "bindings": { "r2_buckets": [{ "binding": "PHOTOS", "bucket_name": "photos" }] },
      "dependencies": { "some-pkg": "^1.0.0" },
      "files": { "pages/photos.tsx": "...", "db/migrations/0001_photos.sql": "..." }
    }
  ],
  "request": ["photos"],
  "expect": {
    "writes": { "pages/_nav.ts": "...", "...": "..." }
  }
}
```

or `"expect": { "refuse_contains": "pages/_nav.ts" }`.

## Pinned formatting rules

- Nav items insert above the `// outer:nav-items` marker at the marker's own
  indentation: `{ href: "/photos", label: "Photos", icon: Camera },`.
- Nav icons merge into the braces on the `// outer:nav-icons` line, deduplicated
  and sorted byte-wise, joined with `", "`.
- Binding lines insert above the `// outer:bindings` marker at the marker's own
  indentation: `r2_buckets: [{ binding: "PHOTOS", bucket_name: "photos" }],`.
  Object keys sort byte-wise; entries join with `", "`. Keys insert byte-wise.
- `package.json` re-serializes whole: all keys sorted byte-wise, 2-space indent,
  one trailing newline.
- Migrations renumber to the next free numbers after the app's highest existing
  migration, keeping byte-wise path order; numbers pad to at least 4 digits.
