// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// What every network front-end shares: the instrument itself.
//
// A protocol server (HiSLIP, Prologix, VXI-11) owns its wire format and its
// session rules, but the thing on the other side of the adapter is the same
// instrument no matter which protocol reached it. Two pieces follow from that
// and live here rather than in any one protocol module:
//
// - `instrument`: one GPIB device at one primary address, exposed as *split*
//   write/read primitives. Protocols that carry an explicit read request
//   (VXI-11 `device_read`, Prologix `++read`) map straight onto these.
//   Protocols that don't (HiSLIP, where the server pushes replies) build
//   their read-after-write policy on top — the policy is the protocol's,
//   the primitives are not.
//
// - `lock`: the viLock registry. A lock excludes I/O from other clients on
//   the *instrument*, which only works if all front-ends consult the same
//   table.

pub mod instrument;
pub mod lock;
