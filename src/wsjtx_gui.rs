// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Start WSJT-X calling CQ, and press its band and mode buttons, by driving its own
//! controls through Windows UI Automation (PowerShell, no focus or mouse).
//!
//! UDP FreeText is no use for the CQ: it never enables Tx. So the script writes the CQ
//! into the Tx6 box, selects Tx6 and Invokes "Enable Tx". Controls are found by their Qt
//! object names, which are the same in WSJT-X and WSJT-X Improved.

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::hunter::BANDS;

/// wsjtx.exe, and WSJT-X Improved's ws.exe
pub const PROCESS_NAMES: [&str; 2] = ["wsjtx", "ws"];
const QSO: &str = "MainWindow.centralWidget.lower_panel_widget.controls_stack_widget.page.\
QSO_controls_widget.tabWidget.qt_tabwidget_stackedwidget.tab.";

const CQ_SCRIPT: &str = r#"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$names = @(%NAMES%)
$text = '%TEXT%'
$ae = [System.Windows.Automation.AutomationElement]
$scope = [System.Windows.Automation.TreeScope]::Descendants
function Find($w, $prop, $value) {
  $c = New-Object System.Windows.Automation.PropertyCondition($prop, $value)
  return $w.FindFirst($scope, $c)
}
$result = 'notfound'
foreach ($proc in Get-Process | Where-Object { $names -contains $_.ProcessName }) {
  $cond = New-Object System.Windows.Automation.PropertyCondition($ae::ProcessIdProperty, $proc.Id)
  foreach ($w in $ae::RootElement.FindAll([System.Windows.Automation.TreeScope]::Children, $cond)) {
    $box = Find $w $ae::NameProperty 'Enable Tx'
    if ($box -eq $null) { continue }
    $edit = Find $w $ae::AutomationIdProperty '%QSO%tx6'
    if ($edit -ne $null) {
      $v = $edit.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
      if ($v.Current.Value -ne $text) { $v.SetValue($text) }
    }
    $rb = Find $w $ae::AutomationIdProperty '%QSO%txrb6'
    if ($rb -ne $null) {
      $rb.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern).Select()
    }
    $b6 = Find $w $ae::AutomationIdProperty '%QSO%txb6'
    if ($b6 -ne $null) {
      $b6.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
    }
    $t = $box.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
    if ($t.Current.ToggleState -eq 'On') { $result = 'already' }
    else { $box.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke(); $result = 'enabled' }
  }
}
$result
"#;

/// WSJT-X's own band and mode buttons (Qt object names). Invoke on them retunes the rig
/// / changes mode at once; the band combo's list items and the Mode menu don't work.
pub const MODES: [&str; 3] = ["FT8", "FT4", "FT2"];
const PANEL: &str = "MainWindow.centralWidget.lower_panel_widget.";

const PRESS_SCRIPT: &str = r#"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$names = @(%NAMES%)
$ae = [System.Windows.Automation.AutomationElement]
$result = 'notfound'
foreach ($proc in Get-Process | Where-Object { $names -contains $_.ProcessName }) {
  $cond = New-Object System.Windows.Automation.PropertyCondition($ae::ProcessIdProperty, $proc.Id)
  foreach ($w in $ae::RootElement.FindAll([System.Windows.Automation.TreeScope]::Children, $cond)) {
    $c = New-Object System.Windows.Automation.PropertyCondition($ae::AutomationIdProperty, '%ID%')
    $b = $w.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $c)
    if ($b -eq $null) { continue }
    $b.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
    $result = 'ok'
  }
}
$result
"#;

fn names() -> String {
    PROCESS_NAMES.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(",")
}

/// Object name of WSJT-X's button for `band` ("20m" -> "...pb20"), or None.
pub fn band_button(band: &str) -> Option<String> {
    BANDS.iter().any(|b| b.0 == band).then(|| format!("{PANEL}pb{}", &band[..band.len() - 1]))
}

/// Object name of WSJT-X's button for `mode` ("FT8" -> "...ft8Button"), or None.
pub fn mode_button(mode: &str) -> Option<String> {
    MODES.contains(&mode).then(|| format!("{PANEL}{}Button", mode.to_lowercase()))
}

/// WSJT-X's default dial frequency (Hz) for `band` in `mode`; None when unknown (FT2 has
/// no table here).
pub fn dial_hz(band: &str, mode: &str) -> Option<u64> {
    let (ft8, ft4): (u64, u64) = match band {
        "160m" => (1_840_000, 1_840_000),
        "80m" => (3_573_000, 3_575_000),
        "60m" => (5_357_000, 5_357_000),
        "40m" => (7_074_000, 7_047_500),
        "30m" => (10_136_000, 10_140_000),
        "20m" => (14_074_000, 14_080_000),
        "17m" => (18_100_000, 18_104_000),
        "15m" => (21_074_000, 21_140_000),
        "12m" => (24_915_000, 24_919_000),
        "10m" => (28_074_000, 28_180_000),
        "6m" => (50_313_000, 50_318_000),
        "2m" => (144_174_000, 144_170_000),
        _ => return None,
    };
    match mode {
        "FT8" => Some(ft8),
        "FT4" => Some(ft4),
        _ => None,
    }
}

/// Press WSJT-X's button for `band`. Ok("ok"), Ok("unknown") (not a band we know) or
/// Ok("notfound"); Err if PowerShell can't be run.
pub fn set_band(band: &str) -> Result<String, String> {
    match band_button(band) {
        Some(id) => press(&id, 20),
        None => Ok("unknown".into()),
    }
}

/// Press WSJT-X's button for `mode`; results as for set_band.
pub fn set_mode(mode: &str) -> Result<String, String> {
    match mode_button(mode) {
        Some(id) => press(&id, 20),
        None => Ok("unknown".into()),
    }
}

fn press(ident: &str, timeout: u64) -> Result<String, String> {
    run(&PRESS_SCRIPT.replace("%NAMES%", &names()).replace("%ID%", ident), timeout)
}

/// Put `text` in WSJT-X's Tx6, select it and switch Tx on. Ok("enabled"), Ok("already")
/// (Tx was on) or Ok("notfound"); Err if PowerShell can't be run.
pub fn start_cq(text: &str) -> Result<String, String> {
    let script = CQ_SCRIPT
        .replace("%NAMES%", &names())
        .replace("%TEXT%", &text.replace('\'', "''"))
        .replace("%QSO%", QSO);
    run(&script, 15)
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn run(script: &str, timeout: u64) -> Result<String, String> {
    let mut child = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > Duration::from_secs(timeout) => {
                let _ = child.kill();
                return Err(format!("Command timed out after {timeout} seconds"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let stdout = reader.join().unwrap_or_default();
    Ok(stdout.split_whitespace().last().unwrap_or("notfound").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_and_mode_buttons() {
        assert!(band_button("20m").unwrap().ends_with(".pb20"));
        assert!(band_button("160m").unwrap().ends_with(".pb160"));
        assert!(mode_button("FT4").unwrap().ends_with(".ft4Button"));
    }

    #[test]
    fn dial_frequencies() {
        assert_eq!(dial_hz("40m", "FT8"), Some(7_074_000));
        assert_eq!(dial_hz("20m", "FT4"), Some(14_080_000));
        assert_eq!(dial_hz("20m", "FT2"), None);
    }

    #[test]
    fn unknown_names() {
        assert!(band_button("1m").is_none());
        assert!(mode_button("CW").is_none());
        assert_eq!(set_band("1m").unwrap(), "unknown");
        assert_eq!(set_mode("CW").unwrap(), "unknown");
    }
}
