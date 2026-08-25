# rustykrab-e2e

Every eval runs through here. One binary, one report, one exit code.

```sh
make e2e            # deterministic plumbing scenarios — no model, seconds
make eval           # gemma4:26b behaviour scenarios, 3 repetitions each
make eval-cred      # the credential-ask measurement, per surface
make eval-login     # live-network login flows (opt-in; skips unless RK_LOGIN_* set)
make eval-list      # list every scenario
```

Flags pass through: `make eval ARGS="--case compaction"`, or call
`./scripts/e2e.sh --mode all --release`.

## Three modes, and why they are not one number

| mode | provider | scored by | gates CI |
|---|---|---|---|
| `scripted` | replayed script, no model | boolean assertions | yes — must pass **every** run |
| `model` | real model, stubbed tools | boolean assertions + LLM judge | yes — must pass a **majority** of repetitions |
| `credential` | real model, real tools, empty credential store | outcome **distribution** | no — reports a rate |
| `login` | real model, real tools, **the real internet** | outcome distribution | no — xfail, and opt-in |

The four answer different questions and the report never merges them.

**scripted** answers *"does the server still do what it says"*: auth, the
origin allowlist, conversation CRUD, SSE framing, secrets, the credential
guard, device pairing. Deterministic, so one failure in three is a broken
scenario rather than an acceptable rate. This is the mode for CI.

**model** answers *"how does the framework behave with a model in the
loop"*: tool selection, argument fidelity, recovery from a broken tool,
what survives compaction, whether memory comes back. Sampled, so it is
scored as a pass rate and flagged `~flaky` when repetitions disagree.

**credential** answers *"when the agent needs a credential it does not
have, does it ask over a protocol the user can answer on"*. That question
has no single right answer — it has a distribution over outcomes, per
surface — so these scenarios are `Expected::Measure`: they report and never
turn the suite red.

**login** answers *"can the agent get into a provider it has never seen, and
then use what is behind the door"*. It carries the credential loop past the
ask: the request is answered with real credentials and the question becomes
whether the agent then signs in and finishes the job. Two scenarios,
deliberately separate — signing in and using what is inside fail
independently, and one scenario covering both cannot say which broke.

This is the only mode that leaves the machine, so it is opt-in
(`RK_LOGIN_URL`/`RK_LOGIN_USER`/`RK_LOGIN_PASS`), excluded from `--mode all`,
and `Expected::XFail` until the capability lands. A fixture would not do:
pointed at a local one, the agent read the password out of the fixture's own
source with its filesystem tools and "signed in" without asking. Anything the
harness can reach on this machine, so can the agent under test. Use a
throwaway account.

### Why `Measure` exists

`xfail` is boolean by construction: a scenario either passed or it did
not, and an unexpected pass turns the suite red so it gets promoted. A
*rate* has no such thing. Forcing the credential suite into pass/fail
would throw away the distribution that is its entire product, and a
threshold pretending to be an xfail would make the report lie about what
was measured. So there is a third expectation. Give a measured scenario a
threshold when you want it to gate; until then it reports.

## Surfaces

The credential suite runs every scenario on every surface — `gateway`,
`telegram`, `signal` — because the agent behaves differently on each, and
a behavioural result that does not name its surface is not a result. A
capture server stands in for the Telegram Bot API and signal-cli, so a
trial can read what the bot *would* have sent with no network egress. It
answers both channels' long-poll shapes, which matters: reply with the
wrong one and the daemon's poll loop logs a decode error every second for
the length of the trial.

## Scripting the model, scripting the tools

`ScriptedProvider` scripts the **model** so the plumbing can be tested
without one. `RUSTYKRAB_TOOL_STUBS` is the mirror image: it scripts the
**tools** so a real model can be tested against situations a live tool
would only reach by luck — an upstream that times out once and then works,
one that never works, a search that legitimately returns nothing, a result
larger than the context window.

```json
{
  "mode": "replace",
  "keep": ["memory_search", "memory_save"],
  "tools": [{
    "name": "weather_lookup",
    "description": "Get the current weather for a city.",
    "parameters": { "type": "object",
                    "properties": { "city": { "type": "string" } },
                    "required": ["city"] },
    "script": { "responses": [
      { "type": "err", "message": "upstream timed out", "kind": "timeout" },
      { "type": "ok",  "value": { "temperature_c": 17, "conditions": "sleet" } }
    ]}
  }]
}
```

`responses` is indexed by call number; `repeat_last: false` makes further
calls fail, which is how a scenario checks that an agent *stops* retrying.
Error kinds map onto `ToolErrorKind` because the agent loop branches on
them — without the mapping a "timeout" scenario would quietly exercise the
generic error path instead.

The credential suite deliberately uses **no** stubs: its premise is that
the real tools cannot run because the credential they need is absent.

**Registering a tool is not enough for the model to see it.** Schemas are
only sent for tools in the conversation's *active* set —
`DEFAULT_ACTIVE_TOOLS` plus whatever `tools_load` has pulled in — so a
freshly registered tool is invisible until something activates it. The
harness sets `RUSTYKRAB_ACTIVE_TOOLS` to exactly the stubs each scenario
declares, and the credential suite names the real tool families it reaches
for. Without that a scenario measures tool discovery instead of the thing
it is about, and an assertion like "the irrelevant tool was not called"
passes for the wrong reason.

## How assertions see a run

Out of the daemon's SQLite, not the REST API. The API speaks an
app-facing shape — `{role, content}` with content flattened to a string —
so tool calls, tool arguments, tool results and the whole compaction state
are simply not in it. `conversations.data` and the `messages` rows have
all of it, which is also how the credential suite reads
`credential_requests`.

That is what makes `ToolOutputContainsAny` possible: a memory scenario can
check that retrieval returned the fact, separately from whether the model
then used it well.

An LLM judge covers only what substring matching cannot — whether an
answer admitting a tool failed is an honest report or a hedge around an
invented number. `claude-sonnet-5` grades when `ANTHROPIC_API_KEY` is set,
the model under test grades itself otherwise, and the report names which.
The judge only runs on repetitions whose hard assertions already passed.

## Compaction, and the lever that actually moves it

Compaction fires when the conversation crosses
`compaction_threshold_pct` of `effective_context_limit()`. That limit is
whatever the **provider** reports, falling back to the profile's
`max_context_tokens` only when the provider reports nothing. Ollama always
reports, so setting `max_context_tokens` in `harness.toml` does nothing —
the scenarios set `RUSTYKRAB_NUM_CTX` instead, which is what that function
reads.

One window serves the whole model suite rather than one per scenario:
Ollama unloads and reloads 17GB of weights whenever the window changes,
and varying it per scenario spent more time reloading than running.

Compaction here does not leave a bookmark. It **replaces** the live
messages with a model-written summary plus a continuation turn, and
archives the displaced history into `recall_archive` for the recall tools.
So a scenario detects compaction from the continuation turn — the summary
is model-written and could say anything — and checks that nothing was
destroyed against the archive, not against a message count that is now
expected to shrink.

Facts that must survive are stated in the first turn so they land in the
displaced region; a fact still in the live window proves nothing about the
summary.

## Surviving a killed run

A full credential run is hours long and the summary is only written at the
end. Every trial is appended to `e2e-credential-trials.jsonl` the moment it
finishes, and `--resume` replays what is already there instead of paying
for it twice. Without `--resume` the file is truncated, so a fresh run
never silently inherits an old one's trials.

## Prerequisites

`scripted` needs nothing but a build. `model` and `credential` need Ollama
running with the model pulled (`ollama pull gemma4:26b`).

Preflight checks that up front, including one trivial generation: Ollama
serves one request at a time per model, so another client holding the slot
— a running RustyKrab daemon is the usual culprit — does not slow the
suite down, it stops it. Better to fail in one line than to time out every
scenario twenty minutes in.

## Known limits

- Scenarios run sequentially. One 26B model behind one GPU means
  concurrency would only make each scenario slower and the timings
  meaningless.
- `model` and `credential` boot a fresh daemon per repetition. That costs
  a few seconds each and buys the only thing that makes repetitions
  meaningful: no scenario can pass on state another left behind.
- Skills are not installed into the throwaway data dir, so the system
  prompt carries no skill catalog. Nothing in the current scenarios
  depends on one.
- `E2E_DAEMON_LOG` sets the daemon's `RUST_LOG`. The spawn clears the
  environment, which clears `RUST_LOG` with it, so a misbehaving scenario
  otherwise leaves a log with nothing in it to explain why.
