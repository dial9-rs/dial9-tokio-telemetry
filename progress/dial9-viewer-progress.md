# dial9-viewer MVP Progress

## Status: In Progress

## Task 1: Move UI files
- [ ] Move files
- [ ] Update symlink
- [ ] Update all references
- [ ] Verify tests pass

## Task 2: Scaffold crate
- [ ] Cargo.toml + workspace
- [ ] main.rs with clap
- [ ] axum static file serving
- [ ] placeholder browser.html
- [ ] serve.py
- [ ] integration test

## Task 3: StorageBackend + /api/search
- [ ] StorageBackend trait
- [ ] S3Backend impl
- [ ] /api/search endpoint
- [ ] integration test with s3s-fs

## Task 4: /api/trace endpoint
- [ ] get_object on trait
- [ ] gunzip + concat logic
- [ ] 50MB cap
- [ ] integration test

## Task 5: browser.html
- [ ] search box + bucket input
- [ ] results table with checkboxes
- [ ] View Selected → opens viewer
- [ ] fallback when no backend

## Task 6: Cleanup
- [ ] README.md
- [ ] clippy clean
- [ ] fmt clean
- [ ] nextest all green
