# PR: Add offline symbolizer with schema-based proc maps and symbol table

Part of https://github.com/dial9-rs/dial9-tokio-telemetry/issues/24

### Summary

Adds the building blocks for moving symbolization off the flush thread and into a background process. Today `inline_callframe_symbols` resolves every callframe address synchronously via blazesym (DWARF parsing, file I/O), blocking trace flushing.

This PR adds:

1. `ProcMapsEntry` and `SymbolTableEntry` event types defined with `derive(TraceEvent)` in `perf-self-profile`, using the existing schema/event system rather than dedicated wire frame types. Any decoder that understands schemas automatically handles them without custom parsing code.
2. An offline `symbolize_trace()` function in `perf-self-profile` that reads a trace with `ProcMapsEntry` events and `StackFrames` fields, resolves addresses via blazesym (including inlined functions), and appends `SymbolTableEntry` events to the output.
3. `resolve_symbols_with_maps()` in `perf-self-profile`, which returns all symbols at an address including inlined callees (the existing `resolve_symbol_with_maps` delegates to it for backwards compatibility). Both userspace and kernel paths handle inlined functions.

Does not change the telemetry crate or `TOKIOTRC` format. Wiring into the background worker pipeline and format migration are follow-up work.

#### Design: schema-based events over dedicated frame types

ProcMaps and SymbolTable data are represented as regular `derive(TraceEvent)` structs rather than custom wire frame types. The core wire format now only has Schema, Event, StringPool, and TimestampReset.

The main motivation is backwards compatibility. Dedicated frame types require every decoder to add custom parsing code for each new type, and there is currently no way to evolve them additively. Schema-based events only require decoders to understand the generic schema/event mechanism. New data shapes (like proc maps or symbol tables) can be introduced without any decoder changes. Consumers that care about the data match on the schema name; those that don't simply see it as another event and skip it.

These events carry a timestamp of 0 (the data is metadata, not a trace event in the temporal sense). The offline symbolizer writes them using low-level codec functions since it appends to an existing trace rather than using the high-level `Encoder`.

### Testing

- Round-trip tests for `ProcMapsEntry` and `SymbolTableEntry` schema-based events
- Symbolizer tests: empty trace, missing proc maps, missing stack frames, unresolvable addresses, full end-to-end flow with real addresses
- `parse_proc_maps_filters_correctly` test for the maps parser
- JS decoder, fuzz target, and integration tests updated
