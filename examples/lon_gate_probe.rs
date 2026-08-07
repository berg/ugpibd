// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Hardware probe for the 82357 read-refusal contract (docs/CAPTURE.md §14.18).
// Needs an 82357A/B connected; needs NO instruments on the bus.
//
// The claim under test: a read issued while the adapter does not consider
// itself a listener is refused *immediately* with a trailing flags byte of
// ATRF_UNADDRESSED, while a read issued with listen-only raised is armed and
// runs to the host timeout on a quiet bus. If both halves hold, the two
// capture failure modes are distinguishable from the host side and the
// §14.18 logging is trustworthy.
//
//   cargo run --example lon_gate_probe

use ugpibd::backend::agilent_82357::gpib::GpibController;
use ugpibd::backend::agilent_82357::protocol::*;
use ugpibd::backend::agilent_82357::usb;
use ugpibd::backend::agilent_82357::MODEL_82357B;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("debug")
        .with_writer(std::io::stderr)
        .init();

    let timeout_ms = 1500;
    let transport = usb::initialize_device(&MODEL_82357B, timeout_ms, None).await?;
    let mut ctrl = GpibController::new(transport, timeout_ms);
    let _ = ctrl.abort(true).await;
    let _ = ctrl.abort(false).await;
    ctrl.init(0).await?;
    println!("init ok; bus lines: {}", ctrl.bus_lines().await?);

    // Phase 1: raise listen-only, issue a NO_ADDRESS read on the silent bus.
    // Expected: the read is ARMED — it should run the full host timeout and
    // come back as a timeout error, not as an instant empty completion.
    ctrl.set_listen_only(true).await?;
    println!("listen-only raised; bus lines: {}", ctrl.bus_lines().await?);
    let t = std::time::Instant::now();
    let r = ctrl.read(0, 4096).await;
    let elapsed = t.elapsed().as_millis();
    match &r {
        Err(e) => println!("phase 1: read ended in error after {elapsed} ms: {e}"),
        Ok((data, eom)) => println!(
            "phase 1: read completed after {elapsed} ms: {} bytes, eom={eom}",
            data.len()
        ),
    }
    let armed = elapsed >= timeout_ms as u128;
    println!(
        "phase 1 verdict: {} (gate up -> armed read on quiet bus)",
        if armed {
            "ARMED, as predicted"
        } else {
            "NOT ARMED — prediction failed"
        }
    );

    // Phase 2: drop the listener state with a bare AUX_LON (0x09) while
    // leaving `listen_only` set so read() skips addressing. Expected: the
    // adapter refuses the read immediately, zero bytes, trailing
    // ATRF_UNADDRESSED (visible as the §14.18 warn line above this output).
    ctrl.write_registers(&[RegisterPairlet {
        address: TMS_AUXCR,
        value: AUX_LON,
    }])
    .await?;
    println!("listener state dropped (AUXCR=0x09), listen_only left set");
    let t = std::time::Instant::now();
    let r = ctrl.read(0, 4096).await;
    let elapsed = t.elapsed().as_millis();
    match &r {
        Err(e) => println!("phase 2: read ended in error after {elapsed} ms: {e}"),
        Ok((data, eom)) => println!(
            "phase 2: read completed after {elapsed} ms: {} bytes, eom={eom}",
            data.len()
        ),
    }
    let refused = r.is_ok() && elapsed < 500;
    println!(
        "phase 2 verdict: {} (gate down -> instant refusal)",
        if refused {
            "REFUSED INSTANTLY, as predicted"
        } else {
            "NOT the predicted refusal"
        }
    );

    // Restore: leave the adapter a plain controller again.
    ctrl.set_listen_only(false).await?;
    println!("restored; bus lines: {}", ctrl.bus_lines().await?);
    Ok(())
}
