//! # floptle-input
//!
//! Bind any number of physical inputs (keys, mouse buttons, gamepad buttons/
//! axes) to a named **action**; scripts ask "is `Jump` pressed?" and never
//! touch raw devices. See `docs/subsystems/input.md`.
//!
//! Planned modules:
//! - `device`  : keyboard / mouse / gamepad sources (winit + gilrs).
//! - `action`  : action map (name -> bindings), pressed/held/released states.
//! - `axis`    : 1D/2D axes (WASD, sticks, triggers) with deadzones.
//! - `context` : input contexts (gameplay vs. menu vs. cutscene) + priority.
