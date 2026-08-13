# The replay harness

`--replay <file.jsonl>` is §12 of the v2 routing spec: the thing that turns
`confidence_threshold` and `reserve_threshold` from guesses into measurements.
It scores captured conversations with **no model calls and no server** — which
is the whole reason `route/signals.rs` and `route/scorer.rs` were kept pure.

```sh
tmuxlet-server --replay tests/fixtures/replay/error-recovery.jsonl
cat tests/fixtures/replay/*.jsonl > /tmp/corpus.jsonl   # sweep the lot at once
tmuxlet-server --replay /tmp/corpus.jsonl
cargo test --test replay                                # the guards
```

It exits non-zero on an unreadable file, a malformed line, or a file with no
conversations. Output is one line per conversation, then the sweep:

```
conversation hard-error-recovery: turns=8 score=+0.762 confidence=0.762 signals=w/e/r/t=0/0/2/0 sev=0.70 depth=8 compacted=false
threshold 0.50: keep=2 shed=0
```

`score` is signed — positive leans capable, negative leans efficient — and
`confidence` is its magnitude, which is what gets compared against
`confidence_threshold`. A conversation is counted **keep** when
`confidence >= threshold && score > 0.0`, and **shed** otherwise.

## Adding a captured conversation

Drop a `.jsonl` file in `tests/fixtures/replay/`, one JSON object per line:

```json
{"id": "some-run", "note": "what this exercises", "messages": [ ... ]}
```

`messages` is the verbatim OpenAI-shaped array — exactly what `signals::extract`
reads — so capture the request body as it arrived. **Scrub paths and anything
else identifying before committing.**

Two conventions the tests enforce. The parser is **strict JSONL**: blank lines
are skipped and nothing else is, so a `#` or `//` comment line is a parse error
— prose goes in the ignored `note` field instead. And every fixture's numbers
are pinned in `EXPECTED` in `tests/replay.rs`. When you add a fixture, run it,
read the output, and add its row. A fixture that silently scores 0.0 when it was
written to look like a spinning run is worse than no fixture, and `EXPECTED` is
the only thing that would notice.

## Reading the four quadrants — for our cost model, not Switchyard's

| Quadrant | Meaning | What it tells us |
|---|---|---|
| `RESCUE` | leg 0 passes, shed leg fails | quota well spent — protect these turns |
| `LOSS` | leg 0 fails, shed leg passes | over-reserving; leg 0 is not always better |
| `SAFE` | both pass | the rationing budget |
| `HARD` | both fail | routing is irrelevant |

The table is Switchyard's; the reading is not. §12: "For Switchyard, `SAFE` is
'cheap tier is free money' — every SAFE turn routed cheap is dollars saved, so
you want to maximize them. For us, `SAFE` is *quota you may spend elsewhere*:
shedding a SAFE turn **costs** money and only makes sense when headroom is
scarce. `SAFE` is not a win to be harvested, it is the reserve you draw down
under pressure." That is a linear cost curve versus a step function (§7.3), and
it moves the calibration target with it:

- **Switchyard's:** the lowest threshold covering RESCUE without inflating LOSS
  — minimize spend subject to quality.
- **Ours:** the `reserve_threshold` at which shedding the SAFE set is *just*
  enough to keep leg 0 available for the RESCUE set through the end of the
  window — minimize spend subject to **not running out**.

Their ~20% escalation result answers a different question and is not our
baseline. **Our null hypothesis is `reserve_threshold = 1.0`** — never ration,
always leg 0, fall through only on an actual 429. Today's behavior, free to
measure, and per §13 possibly correct.

## What this harness cannot do

**It has no ground truth about whether a turn succeeded on either tier.** It
knows what the router *would decide* and nothing about what happened next, so it
cannot populate the quadrants from fixtures alone — each is defined by a
pass/fail pair and the harness has neither half. It gives you the routing split;
the quadrants need conversations actually run both ways. Closing the gap needs,
per turn: the request body, the leg that served it, the leg that *would* have
served it under the other setting, and an outcome label for each — did the turn
produce the artifact the caller wanted. That label is the expensive part and the
only one that cannot be reconstructed after the fact. Until it exists, the sweep
describes the router's behavior; it is not evidence that any threshold is right.

Four smaller limits, all measured rather than assumed:

- **The sweep is only §7.7 rung 6.** It compares confidence against the
  threshold and nothing else, so it does not apply the unconditional overrides
  that sit *above* the score in the cascade. `compacted-then-settled`,
  `compacted-user-envelope` and `critical-connection-refused` all trip
  `should_escalate` and would take leg 0 at rung 3 no matter what, yet the sweep
  reports them as shed. Three of the ten fixture conversations are affected —
  read the split as "what the score alone would do", not as the router's
  decision.
- **A score of exactly 0.0 always sheds**, because keeping requires
  `score > 0.0`. So the tool-free fixture reports `keep=0 shed=1` at every
  threshold, which reads as a contradiction of §7.8's "scores 0.0 and lands on
  leg 0 every time". Both are right about different regimes: §7.8 describes
  rung 4, the headroom path where every turn starts at leg 0; the sweep models
  the rationing path, where §7.6 sends an indecisive turn one leg deeper. The
  output does not currently say which regime it is reporting.
- **A tool-free conversation scores 0.0 only while it is shallow.** Past
  `STALL_MIN_TURN_DEPTH = 8`, `no_prod` is trivially true and `investig`
  trivially false, so `spinning` fires and a long plain chat scores `+tanh(0.5)`.
  `plain-chat-deep.jsonl` is that counterexample, pinned rather than fixed.
- **The dial has fewer settings than it looks.** The scorer emits a small set of
  discrete values — the whole fixture corpus produces only {0.000, 0.245, 0.462,
  0.613, 0.762} — so thresholds 0.0 and 0.3 give an identical split, as do 0.5
  and 0.7. Sweeping more finely reveals nothing until real captured traffic is
  in the corpus.
