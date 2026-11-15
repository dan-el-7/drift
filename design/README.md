# Drift design system

One source of truth for both native UIs (WinUI 3 on Windows, GTK4/libadwaita on Linux).

- `tokens.json`: colours (black/white + two greys; light theme is the inverse of dark),
  type (Geist, Inter fallback; tabular numerals; tight tracking on display sizes and the PIN),
  stroke widths, spacing scale, window size.
- `motion.json`: named springs, loops, the reduced-motion fallback, and how throughput maps to
  particle speed on the phone→PC line.
- `src/lib.rs` (`drift-design` crate): loads both files and converts springs for each toolkit.

## Springs

Every spring is **damping ratio + response** (the undamped period, in ms). That's what a designer
tunes: "how bouncy" and "how quick".

| Toolkit | API | Mapping |
|---|---|---|
| Windows | `SpringScalarNaturalMotionAnimation` / `SpringVector3NaturalMotionAnimation` | `DampingRatio` = damping ratio, `Period` = response |
| Linux | `AdwSpringAnimation` + `AdwSpringParams` | damping ratio, mass 1, stiffness = (2π / response)² |
| Linux custom widgets | `Spring::value_at(t)` reference solver | closed form; same curve as the above |

Rules: animate transforms and opacity only; every state change (idle → found → PIN →
transferring → done) is one continuous motion, never a cut; when the OS asks for reduced motion,
use a 160 ms crossfade and no loops.

## Signature moments (to design, not fixed)

| Moment | Spring | Idea |
|---|---|---|
| Idle | `breathe` + sonar loop | a single dot breathing, faint hairline rings while visible |
| Device found | `settle` | a ring condenses into a labelled node that slides into place |
| PIN | `settle` | digits resolve into crisp tabular numerals |
| Transfer (single file) | `settle` | the user's idea: a hollow ring that eats itself into a solid ball as it fills |
| Transfer (path) | n/a | hairline phone→PC line, particles at real throughput; straight for Direct, bent through a router glyph for LAN |
| Done | `puff`, `drop` | the ball puffs out edge to edge; received files drop in with a soft overshoot |

The transfer layer must never block the UI: selecting or dropping more files keeps working while it plays.
