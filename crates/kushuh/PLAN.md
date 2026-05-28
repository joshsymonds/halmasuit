# kushuh — PLAN

The concrete build sequence for `kushuh`'s current phase. The full
design — role/perspective model, the star-map navigator, the
nested-compositor integration with halmasuit, prior art, and the
five-phase roadmap — lives in **`ARCHITECTURE.md`**; this document does
not duplicate it. PLAN.md tracks *what we are building right now and in
what order*. When a phase ships, the relevant section here is replaced
by a README describing the as-built crate.

---

## Where we are

**Phase 1: the config domain model.** Before any rendering code exists,
`kushuh` defines the data structures the entire compositor manipulates —
roles, perspectives, regions, the top-level layout — and proves the
model is coherent by parsing and validating the example configs from
`ARCHITECTURE.md`. This attacks *design* risk (the novel role model)
rather than *technical* risk (smithay is well-trodden; that is Phase 2).

The crate is deliberately **lib-only** for this phase and pulls in no
Wayland / smithay / GPU dependencies. The model is pure data: fast to
test, no VM, no GPU, no I/O beyond reading a KDL string. The `[[bin]]`
compositor entry point lands in Phase 2.

`unsafe` is intentionally *not* forbidden at the crate root — Phase 2's
direct-DRM / libinput FFI will need it under justified per-item
`#![allow]`s. Until then the workspace `unsafe_code = "warn"` lint
surfaces any unsafe rather than silently accepting it.

---

## Build sequence

Built bottom-up, test-first (TDD), one type per task. Each task is a
gambit checkpoint: the genuine open modeling forks (below) are resolved
just-in-time at the type that raises them, rather than in an upfront
design pass.

1. **`Region`** ✅ — the atomic geometry leaf: a validated rectangle in
   monitor-percentage space (`x`/`y`/`width`/`height` as `0..=100`,
   non-zero area, fits the monitor).
2. **`Role`** — a named position (monitor + `Region`) with a binding.
   Binding is a sum type; see the binding fork below.
3. **`Perspective`** — a named set of roles. The ambient role is
   implicit/structural, not a member of the roles list.
4. **`Config` / `Layout`** — the top-level model. Separates role
   *definitions* from role *bindings* (see the per-perspective fork).
5. **KDL parser** — add the official `kdl` crate (KDL v2 spec) and
   hand-write the `KdlDocument`→types conversion with span-aware,
   actionable errors. The three `ARCHITECTURE.md` example configs
   (`code` / `meeting` / `reading`) become acceptance fixtures.
6. **Semantic validator** — reject incoherent configs (out-of-bounds /
   zero-area regions caught at `Region` construction; duplicate role
   names within a perspective; perspectives referencing undefined roles;
   bad monitor refs) with clear messages.

---

## Open modeling forks (resolved just-in-time)

The design doc is exhaustive on the *concept*; these are the spots where
it states a "probable default, to be confirmed" or shows two forms.
Each is a 1–2 sentence decision made at the relevant type's checkpoint,
not a re-derivation of the model.

- **Binding has four forms, not three** (decided at `Role`): `bind-app`
  (sticky), `bind-app-candidates [...]` (cycle), no binding (flex), and
  `bind-app-pattern "App" title="<regex>"` (pattern — e.g. the doc's
  Zoom `title="^Meeting$"`). The `Binding` enum needs a `Pattern`
  variant carrying an app id and an optional title regex.
- **Two perspective syntaxes** (decided at `Perspective`): roles *by
  reference* (`perspective "code" { roles "editor" "browser" }`) vs
  roles *defined inline* (`perspective "meeting" { role "x" { … } }`).
  The model must support both, or we pick one.
- **Per-perspective vs global roles** (decided at `Config`): the doc's
  probable default is "shared apps, per-perspective role bindings." The
  definition/binding split keeps both that and per-perspective isolation
  expressible without hard-committing the policy.
- **Cycle vs tabbed** — *deferred to the Phase 3 engine.* The model only
  stores the candidate list; how a role presents multiple candidates is
  an engine decision, not a config-model one.

---

## Out of scope here

Everything past the config model — smithay scaffolding, the role-based
layout engine, the star-map navigator, reactive-wallpaper integration,
the niri-compat / Mutter / PipeWire surfaces, flipping the default
session compositor — is Phases 2–5 in `ARCHITECTURE.md`. Do not pull any
of it into this crate yet.

---

## Anti-patterns

The full list is in `ARCHITECTURE.md` § "Anti-patterns (forbidden)" and
the executing epic. The ones load-bearing for *this* phase:

- **No Wayland / smithay / calloop / rendering / input code.** Pure data
  model only; mixing in rendering defeats the cheap-validation goal and
  drags GPU/VM deps into the model.
- **No derive-macro KDL parser (knus/knuffel) or serde-KDL.** Hand-write
  the conversion over the official `kdl` crate — the repo owns its
  parsing boundaries (clean-room greetd wire types, hand-rolled PAM FFI).
- **No spatial-primitive abstractions as the model's core.** The central
  abstraction is the named role; spatial nudges are a later navigation
  layer, not part of the data model.
- **No `#![forbid(unsafe_code)]` at the crate root** (Phase 2 needs FFI).
- **No `unwrap`/`expect`/`panic!` on user-supplied config input** —
  config errors are user-facing; return `Result` with span-aware
  diagnostics.
