```
            ██████╗ ██╗███╗   ███╗███████╗
           ██╔═══██╗██║████╗ ████║██╔════╝
           ██║   ██║██║██╔████╔██║███████╗
           ██║▄▄ ██║██║██║╚██╔╝██║╚════██║
           ╚██████╔╝██║██║ ╚═╝ ██║███████║
            ╚══▀▀═╝ ╚═╝╚═╝     ╚═╝╚══════╝

      ╌╌╌╌╌╌╌╌╌╌  c o n v e r t e r  ╌╌╌╌╌╌╌╌╌╌
```

<div align="center">

**Office documents in. Editor-ready HTML out.**

A small, stateless HTTP service wrapping a LibreOffice + poppler pipeline.

</div>

---

## What it does

Most converters give you _semantic_ HTML — the words survive, the layout does not.
This one keeps the page: geometry, pagination and footers come out of LibreOffice's
own rendering, so an imported document still breaks where its author broke it.

```
    .doc   .docx   .odt   .rtf                    .pdf
       └────────┬────────┘                          │
                ▼                                   ▼
      ┌───────────────────┐              ┌───────────────────┐
      │    LibreOffice    │              │      poppler      │
      │   soffice → html  │              │  pdftohtml/-text  │
      └─────────┬─────────┘              └─────────┬─────────┘
                └────────────────┬────────────────┘
                                 ▼
                       ┌───────────────────┐
                       │   editor-ready    │
                       │       HTML        │
                       └───────────────────┘
                    page geometry · pagination
                       · salvaged footers
```

---

## ⚠ Security

**This service has no authentication, and its CORS policy is allow-any. (for now)**

It runs headless LibreOffice over caller-supplied files — a large and historically
sharp parsing surface. Never expose it through an ingress or a published port.

Run it on an internal network and put an authenticated application in front of it,
so that application enforces auth on every conversion. It is designed to be the
back half of a pipeline, never the front door.

---

## API

### `POST /convert?name=<filename>`

Body is the raw file bytes (`Content-Type: application/octet-stream`). The `name`
query parameter supplies the extension, which is what selects the pipeline.

```bash
curl -X POST "http://127.0.0.1:8787/convert?name=policy.docx" \
     -H "Content-Type: application/octet-stream" \
     --data-binary @policy.docx
```

```jsonc
{
  "html": "<?xml version=\"1.0\"…", // full document, head styles included
  "page": { "w": 21.0, "h": 29.7, "t": 2.5, "r": 2.0, "b": 2.0, "l": 2.0 },
  "pages": ["first text of page 2", "…of page 3"],
  "footers": { "first": null, "default": "Page 1 of 4" },
  "original": "1785476834826430824-policy.docx" // or null — see below
}
```

`page` is centimetres — width, height, then top/right/bottom/left margins. It is
`null` when geometry cannot be derived. `pages` carries first-text snippets for
pages 2…N, which is how a caller reconstructs hard page breaks. The PDF path
returns HTML only: `page` is `null`, `pages` is `[]`, `footers` is `{}`.

Accepted: `.pdf` `.doc` `.docx` `.odt` `.rtf`

> ### ⚠ It writes your upload to disk
>
> Every conversion also saves the **untouched original** to
> `$QIMS_DATA_DIR/originals/<unix-nanos>-<sanitised-name>` and returns that
> filename as `original`. This happens in **convert-only mode too** — it is not
> gated by any flag, and nothing ever cleans it up.
>
> So the service is stateless in that it holds no database and keeps no session,
> but it is **not** side-effect free: uploaded documents accumulate on its
> filesystem indefinitely, inheriting the confidentiality of whatever people
> import. Give it a writable volume you are willing to treat as document storage.
> `original` is `null` when the save fails; a failed save never fails the
> conversion.

### `GET /health`

Returns `ok` without touching the conversion pipeline. Point your liveness probe here.

---

## Configuration

| Variable            | Default          | Meaning                                              |
| ------------------- | ---------------- | ---------------------------------------------------- |
| `QIMS_CONVERT_ONLY` | _(unset)_        | `1` serves only `/convert` + `/health` — no database |
| `QIMS_BIND`         | `127.0.0.1:8787` | Listen address. Containers need `0.0.0.0:8787`       |
| `QIMS_LOG`          | `info`           | Log level                                            |
| `QIMS_DATA_DIR`     | `./data`         | Scratch + saved originals. **Used in every mode**    |

Legacy mode only — ignored when `QIMS_CONVERT_ONLY=1`:

| Variable           | Default             | Meaning                          |
| ------------------ | ------------------- | -------------------------------- |
| `QIMS_DB`          | local SurrealDB     | Database endpoint                |
| `QIMS_DB_USER`     | `root`              | Database user                    |
| `QIMS_DB_PASS`     | `root`              | Database password                |
| `QIMS_ADMIN_EMAIL` | `admin@example.com` | Bootstrap admin — **always set** |

### Two modes

The binary predates its current job. With `QIMS_CONVERT_ONLY=1` it is just the
converter — no database connection, only `/convert` and `/health`. This is how you
almost certainly want to run it. Without that flag it also serves the original
documents/users/notifications stack against SurrealDB, and identifies callers from
an `x-qims-user-email` header set by an authenticating proxy (no header at all is
treated as trusted local context and granted admin — another reason never to
expose it directly).

---

## Integrating it

The intended shape, and the one the security note assumes:

```
   browser ──▶ your app (authenticates, authorises)
                   │  POST /convert?name=…   raw bytes
                   ▼
              qims-converter        ◀── internal network only
```

Your application owns auth, size limits and error presentation; the converter owns
the pipeline. Things worth knowing before you wire it up:

- **The body is raw bytes, not multipart.** If your framework caps raw request
  bodies (Django's `DATA_UPLOAD_MAX_MEMORY_SIZE` defaults to 2.5 MB, for instance),
  accept multipart at your own edge and stream the file through as octet-stream.
- **Conversion is slow and memory-hungry.** LibreOffice wants ~300–500 MB per
  document and can take seconds. Give the container its own memory limit rather
  than letting it share one with your app, connect with a short timeout but read
  with a generous one, and expect to bound concurrency yourself.
- **Errors come back as `{"error": "…"}`** with a 4xx/5xx status. Surface that
  message rather than your HTTP client's generic one — it usually names the real
  problem with the document.
- **Give it a writable volume** for `$QIMS_DATA_DIR` (see the disk note above).

---

## Running

### Docker

```bash
docker build -t qims-converter .
docker run -d -p 8787:8787 qims-converter
```

The image carries the whole toolchain and defaults to convert-only on `0.0.0.0:8787`.
`Dockerfile.prebuilt` is the same runtime minus the Rust build — it copies a binary
you compiled yourself, trading a slow `cargo build` for a `COPY`. Mind the glibc
floor documented in its header.

> Image weight is ~1 GB either way. LibreOffice is the bulk, not the Rust binary.

### From source

```bash
cargo build --release
QIMS_CONVERT_ONLY=1 QIMS_BIND=127.0.0.1:8787 ./target/release/qims-backend
```

Needs on `PATH`: `soffice` (libreoffice-writer), `pdftohtml` (poppler-utils),
`unzip`, and `magick` (ImageMagick 7). Without `magick`, images keep the whitespace
LibreOffice pads around metafiles — everything else still works.

**Fonts are not cosmetic.** Page geometry is derived from LibreOffice's rendering,
so metric-compatible substitutes decide whether page breaks land correctly. Install
Carlito (≈ Calibri), Caladea (≈ Cambria) and Liberation (≈ Arial/Times/Courier), or
imported documents will paginate differently from the original.

---

## License

MIT — see [LICENSE](LICENSE).
