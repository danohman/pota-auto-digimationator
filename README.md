# POTA Auto-Digimationator for WSJT-X

POTA Auto-Digimationator watches WSJT-X decodes and calls POTA activators automatically, the same way a double-click on a decode would. It is a native Windows program written in Rust (egui/eframe).

## What it calls

A decode is a candidate when it's a `CQ …` or an `… RR73` / `… 73` (the sender is about to free up) **and** one of these is true:

- the message is `CQ POTA <call> …`
- the callsign has an active spot on pota.app **on the band WSJT-X is on now**
- the station was heard sending `CQ POTA` on this band earlier today

When a station calls you and the hunter isn't going to answer, for example because you never called them, the log says so and puts an **Answer** button at the start of the line. Clicking it replies exactly as a double-click in WSJT-X would, even while Auto-Hunt is disabled or in Dry run. The QSO is then handled like an auto-call: it's marked worked when it completes, the Log QSO window gets the auto-OK, and it goes to ACLog if that's on. The button works for 5 minutes. A 73 or RR73 sent to you just shows as "sent you 73", with no button, since it needs no reply.

It won't auto-call a station decoded weaker than **-20 dB**. Change that under **Settings > Hunting > Skip stations weaker than** (-24 to -6 dB, or No limit). Skipped stations still show in the log.

It skips stations already worked today (UTC) on the same band and mode **at the same park**. If an activator moves to a new park the same day, it calls them again. When the park isn't known, either for the earlier QSO (WSJT-X's own log doesn't record parks) or for where they are now (not spotted), it doesn't call them again on that band and mode until 0000Z. That list is loaded at startup from today's entries in WSJT-X's log (`wsjtx_log.adi`; WSJT-X and WSJT-X Improved keep theirs in different folders and the newest is used) and from the program's own `state.json`, which remembers the parks it confirmed, QSOs that finished on air but weren't logged, and your skips. It's updated each time a QSO completes or is logged, and cleared at 0000Z.

**Skip** a station you don't want to work: click the **Skip** button on its Candidate or CALLING line, or right-click its row in the Spots table and choose Skip. It won't be called again today on that band, mode and park, and Tx is halted if it was the station being called. Skipped spots move to the bottom of the list with an **S** chip; right-click one to stop skipping it.

By default only Rx moves to the station you call; with Hold Tx Freq checked in WSJT-X, Tx stays wherever you put it. Turn on **Settings > Move Tx to their frequency** to send each call as a Ctrl+double-click instead, which puts Tx exactly on the station. WSJT-X's UDP interface can't set Tx to an offset such as 50 Hz away.

If a cycle has nothing worth calling, the log shows one grey line such as `38 calls decoded, no needed POTA`.

When more than one candidate decodes in the same cycle, it calls the one with the strongest SNR. It never calls while WSJT-X has Tx enabled, so it won't interrupt a QSO or your own CQ. If the station doesn't answer after the set number of calls, it halts Tx and leaves that station alone for 10 minutes. If that station calls you during those 10 minutes (a late answer out of a pileup), it replies straight away and the QSO carries on as normal, unless WSJT-X is busy with another QSO.

## Logging

WSJT-X pops up its Log QSO window after each QSO and only logs it (and tells this program) once OK is clicked. There's no UDP command for that, so:

- When a QSO finishes (RR73 / RRR / 73 exchanged), the station is marked worked straight away. It won't be called again on that band and mode even if the QSO never gets logged. The worked list shows a red cross in the Log column until WSJT-X logs it.
- With **Settings > Logging > Press OK on WSJT-X's Log QSO window** on (the default), the program presses OK whenever that window opens, for any QSO. It does this by sending Enter to the window directly, so it doesn't steal focus. If the window doesn't close after a few tries, the log says so and you can click it yourself.
- This works whether Auto-Hunt is enabled or disabled.

## pota.app spots

Spots are fetched **once a minute at most** by one background thread. Each fetch is kept in a local copy (`spots_cache.json`), and every check reads only that copy. If a fetch fails, the program keeps using the last copy and retries after 10 seconds, then 30 seconds, then every minute until it gets a response. The pota.app chip at the right of the status row shows the spot count, how many are on your current band and mode, and when the next refresh is due.

## ACLog

With **Settings > Logging > ACLog** on, each QSO WSJT-X logs is also sent to N3FJP ACLog through its TCP API (in ACLog, turn on Settings > Application Program Interface > TCP API, default port 1100; set the host and port with **Edit** on that page). The park reference goes into ACLog's `Other` field titled `SIG_INFO` (found automatically) and into the comments as `POTA <ref> <park name>`.

If ACLog already gets QSOs straight from WSJT-X (ACLog's own WSJT-X link or JTAlert), leave this off. ACLog's duplicate check only catches a double entry when the call, date, time on, band, mode and grids all match.

## Hunt counts (optional)

Enter your pota.app username and password under **Settings > Connections > pota.app account** and the Spots list shows how often you've hunted each station and park: the count in parentheses after a call you've worked before, and in front of the park an orange **NEW** chip if you've never hunted it, or the count in parentheses if you have. The counts come from your pota.app account (refreshed every 30 minutes, so today's QSOs appear after you upload your log). The password is stored in plain text in `settings.json` on this computer and sent only to pota.app's sign-in service.

## WSJT-X settings it relies on

It works with both WSJT-X and WSJT-X Improved. In Improved, untick **CQ only** and **Hide FT8 dupes** in the Band Activity window: WSJT-X only acts on a call to a line it is showing, so a hidden line is ignored. If you keep CQ only on, switch off **Settings > Hunting > Call stations after their 73**.

The same instructions are in the app under **Settings > Connections > WSJT-X setup**, along with a live connection indicator and the address and port to listen on.

- Settings > Reporting > UDP Server `224.0.0.2`, port `2237`, **Accept UDP requests** ticked
- **Disable Tx after sending 73** on, so Tx stops at the end of each QSO
- Auto Seq on

## Building and running

Install Rust (https://rustup.rs), then:

```
cargo build --release
```

Run `target\release\pota-auto-digimationator.exe`.

It opens with the button showing **Auto-Hunt Disabled** in red, and **Settings > Dry run** on. Click the button and confirm that you're at your station (you remain the control operator, and fully automated QSOs are prohibited) to turn it green (**Auto-Hunt Enabled**) and watch what it would call; the button adds "(Dry run)" while Dry run is on. Then turn Dry run off to let it transmit. All options are in the **Settings** panel (the gear button at the top right, or Ctrl+,). Changes are saved straight away. The window follows Windows' light or dark app mode; the sun/moon button next to the gear switches it, and **Settings > Appearance** picks Match Windows, Light or Dark.

`settings.json`, `state.json` and `spots_cache.json` live in a `data` folder, and `assets/ding.mp3` (the QSO-logged sound) in an `assets` folder, both in the exe's folder or the first folder above it that holds `assets/ding.mp3`, so a build inside `target\release` uses the files in the project root. Files left in the app folder by older versions are moved into `data` at start-up.

Tests: `cargo test`

## License

Copyright (c) 2026 Dan Muntz (WD4DAN)

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. It is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the [LICENSE](LICENSE) file for the full text.
