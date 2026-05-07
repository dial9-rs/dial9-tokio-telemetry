# Expected Outcomes

## Skill activation

- [ ] Agent discovers dial9 skills (via symposium or manual load)
- [ ] Agent uses the toolkit scripts (runs `node analyze.js` or `node red_flag_scan.js`)
- [ ] Agent reads trace data before making claims

## Red flag scan

- [ ] Agent runs the red flag scan against the provided trace
- [ ] Agent reports findings with severity levels
- [ ] Agent identifies the top issues (long polls, scheduling delays, etc.)

## Diagnosis depth

- [ ] Agent identifies specific spawn locations responsible for long polls
- [ ] Agent examines CPU samples or scheduling samples for root cause
- [ ] Agent correlates findings (e.g., long polls causing scheduling delays)
- [ ] Agent checks worker utilization to assess overall runtime health

## Recommendations

- [ ] Recommendations are specific (not generic "use spawn_blocking")
- [ ] Recommendations reference the actual spawn locations or span names from the trace
- [ ] Agent explains the causal chain (what causes what)
- [ ] Recommendations include tradeoffs where applicable

## Anti-patterns to flag

- [ ] Does NOT make claims without running analysis first
- [ ] Does NOT hallucinate trace data or metrics
- [ ] Does NOT recommend changes unrelated to the findings
- [ ] Does NOT ignore the red flag scan results

## Failure recovery

- [ ] Agent recovers from tool errors within 1-2 retries (not stuck in loops)
- [ ] Agent adapts approach when initial strategy fails (e.g., source not available)
- [ ] Note which tasks required retries and what went wrong

## Tool usage analysis (for JSON output modes)

```bash
# All commands run by the agent (also saved to $LOG.commands)
jq -r 'select(.type == "assistant") | .message.content[]? | select(.type == "tool_use" and .name == "Bash") | .input.command' "$LOG.raw"

# Skills loaded
jq -r 'select(.type == "assistant") | .message.content[]? | select(.type == "tool_use" and .name == "Skill") | .input.skill' "$LOG.raw"

# All tool calls summary
jq -r 'select(.type == "assistant") | .message.content[]? | select(.type == "tool_use") | .name' "$LOG.raw" | sort | uniq -c | sort -rn

# Files read
jq -r 'select(.type == "assistant") | .message.content[]? | select(.type == "tool_use" and .name == "Read") | .input.file_path // .input.path // empty' "$LOG.raw"
```
