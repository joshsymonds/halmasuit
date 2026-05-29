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

## Vocabulary

Two registers, deliberately:

- **The config model speaks concrete.** A **`System`** (a whole-desktop
  layout you switch to — the doc's "perspective", canonically a *system*)
  holds **roles**, each a **region** on a **monitor** with a **binding**.
  These are the words you hand-edit, so they name hardware and apps
  directly.
- **The navigator / star-map layer speaks astronomical** (a system drawn
  as a *star*, a monitor as an *orbit*, a live window as a *planet*, a
  role's stacked windows as a *constellation*), exactly as
  `ARCHITECTURE.md`'s star-map section designs. `planet`/`orbit` are
  view/runtime concepts and never appear in the config types.

`System` is the canonical word in both registers — the bridge. `planet`
(= window) is a runtime concept and does not exist in this crate yet.

---

## Build sequence

Built bottom-up, test-first (TDD), one type per task. Each task is a
gambit checkpoint: the genuine open modeling forks (below) are resolved
just-in-time at the type that raises them, rather than in an upfront
design pass.

1. **`Region`** ✅ — the atomic geometry leaf: a validated rectangle in
   monitor-percentage space (`x`/`y`/`width`/`height` as `0..=100`,
   non-zero area, fits the monitor).
2. **`Role`** ✅ — a named position (monitor + `Region`) with a `Binding`.
   `Binding` is a four-kind sum type; see the resolved binding fork below.
   App references carry an optional `profile` for separate-state instances
   (a `code` Firefox vs a `personal` Firefox).
3. **`System`** ✅ — a named set of roles: one whole-desktop layout you
   switch to. The ambient role is implicit/structural, not a member of
   the roles list. An empty system is valid (visible structure). Roles
   are owned by their system (isolation).
4. **`Config` / `Layout`** — the top-level model: the set of systems and
   the chosen config schema (see the config-schema fork).
5. **KDL parser** — add the official `kdl` crate (KDL v2 spec) and
   hand-write the `KdlDocument`→types conversion with span-aware,
   actionable errors. The three `ARCHITECTURE.md` example configs
   (`code` / `meeting` / `reading`) become acceptance fixtures.
6. **Semantic validator** — reject incoherent configs (out-of-bounds /
   zero-area regions caught at `Region` construction; duplicate role
   names within a system; references to slots/roles that do not exist;
   bad monitor refs) with clear messages.

---

## Open modeling forks (resolved just-in-time)

The design doc is exhaustive on the *concept*; these are the spots where
it states a "probable default, to be confirmed" or shows two forms.
Each is a 1–2 sentence decision made at the relevant type's checkpoint,
not a re-derivation of the model.

- **Binding has four kinds, not three** — RESOLVED at `Role`.
  `Binding` is `Sticky(AppRef)` (launch & own one instance),
  `Cycle(Vec<AppRef>)` (swap between ≥2 candidates), `Pattern { app_id,
  title_regex }` (catch a window the app spawns itself — the Zoom meeting
  window, a reading-titled Firefox — `kushuh` does not launch it), and
  `Flex` (free/scratch, no rule). `Pattern` is distinct from `Sticky`
  because it *catches* rather than *launches*. This extends the doc's
  schema, which listed only three forms.
- **App instances carry a `profile`** — RESOLVED at `Role`. An `AppRef`
  is `app_id` + optional `profile`. A `code` Firefox and a `personal`
  Firefox are one `app_id`, two profiles, launched as separate processes
  with **separate state** (independent history/logins). The model stores
  the label; the profile→launch mapping (`firefox -P code`) is a Phase 3
  runtime concern. This extends the doc's KDL schema with a `profile=`
  property (finalized at the KDL-parser task).
- **Per-system vs global roles** — semantics RESOLVED, structure pending
  at `Config`. Roles are **isolated per system**: each system gets its
  *own* instance (Code's Firefox ≠ Personal's Firefox), not a shared one.
  A resolved `System` therefore *owns* its `Vec<Role>`. The source
  *schema* (how that is written — shared slots + per-system bindings, vs
  fully-inline, vs a role pool by reference) is a parser concern, decided
  at the `Config`/parser task.
- **Config schema / two write-forms** — pending at `Config`. The doc
  shows roles both by reference (`roles "editor" "browser"`) and inline
  (`role "x" { … }`); separately, the same slot shape may hold a
  different app/profile per system. Whatever the source syntax, a
  resolved `System` is `name + Vec<Role>`, so this is a parser/`Config`
  decision, not a `System`-type one.
- **Cycle vs tabbed** — *deferred to the Phase 3 engine.* The model only
  stores the candidate list; how a role presents multiple windows (one
  slot holds *its* instance, not all windows of an app) is an engine
  decision, not a config-model one.

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
