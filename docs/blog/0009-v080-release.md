# amux 0.8: The grand refactor ships

The past month has been spent rebuilding amux's internals from scratch. The previous architecture grew organically across sixty-plus work items, and by the end it had become clear that the three frontends (CLI, TUI, headless) were each implementing business logic in slightly different ways. A flag added in one place wouldn't always reach the others. Config values were resolved through different code paths depending on which frontend you used. The only way to verify parity was to test all three by hand and compare.

v0.8 fixes this by reorganizing the codebase into a strict four-layer architecture. All business logic now lives in a single shared command layer. The CLI, TUI, and headless server are thin presentation shells that translate user input into command calls and render the results. Lower layers can never call upward. If a frontend behaves differently from another, that's a bug enforced by the architecture, not a best-effort promise.

---

```sh
# install or upgrade
curl -s https://prettysmart.dev/install/amux.sh | sh
```

---

## What's better for you

**Identical behavior across frontends.** Every command, flag, and config value is resolved through the same code path regardless of whether you're in the TUI, running a CLI one-liner, or dispatching via the headless API. If it works in one, it works in all three.

**Fewer bugs.** The old architecture allowed subtle drift between frontends that was hard to test for. The new layering makes that class of bug structurally impossible.

**Skills overlay.** One new feature shipped alongside the refactor as a test of adding features under the new architecture. Your global amux skills (`~/.amux/skills/`) can now be mounted into any agent container automatically. Enable it with `--overlay "skill()"`, `amux config set overlays.skills true`, or the `AMUX_OVERLAYS` env var. amux figures out the right container path for each agent. See the [docs](../03-security-and-isolation.md#skills-overlay) for details.

No breaking changes. Your `.amux/config.json`, workflows, headless database, and every CLI invocation work exactly as before. `amux ready` and go.

## What this was like to build

I'll be honest: this was harder than expected. The grand refactor was specified entirely in advance and executed across eight work items (0066-0073) by code agents driven against the spec. The agents did solid work on individual layers, but re-creating an identical feature set and user experience on top of a completely different architecture turned out to be the hard part. Things that worked fine in isolation would break in subtle ways when composed together. It required at least two major mid-layer engine rewrites (the workflow engine and the container runtime) and several follow-up work items (0074, 0077, 0078) to reach full parity.

The core issue is that agents are good at building things, but not great at preserving the feel of a system across a structural change. They can match a spec, but matching the sum of sixty prior work items' accumulated behaviors — the small UX decisions, the edge cases, the implicit contracts between components — requires a kind of holistic awareness that current models don't have. Getting there took a lot of iteration and a lot of careful human review.

That said, it was worth doing. The new architecture makes adding features straightforward: define a command, register it in the catalogue, implement the per-frontend rendering, done. The skills overlay was built in a single work item with no surprises, which is exactly the kind of experience the refactor was supposed to enable.

## What's next

On a personal note: two other projects I was working on in the background — [oasis](https://github.com/prettysmartdev/oasis) (a local-first AI chat app) and [alog](https://github.com/prettysmartdev/alog) (an AI-assisted logging tool) — are both being discontinued. Other people are doing them better, and my heart wasn't really in either of them. I'll be taking a short break from amux development as well. The grand refactor was rewarding but frustrating, and I need to recharge. In the meantime I'll be working on a new amux-adjacent project which I'll release soon.

The refactor was a worthy cause. It taught me a lot about agent-assisted software development — both what works well and what still needs to improve. Agents can write solid code against a clear spec, but software architecture, system-level consistency, and preserving user experience across structural changes are still firmly in the "needs a human" category. For now.

---

Source and issues at [github.com/prettysmartdev/amux](https://github.com/prettysmartdev/amux). More at [prettysmart.dev](https://prettysmart.dev). Feedback and contributions welcome.
