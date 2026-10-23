//! Install unit tests (plan Task 13): the scheduled-task XML (pure) and the
//! UTF-16 LE BOM encoding. `install()`/`uninstall()` drive `schtasks` and the
//! system task store — system-modifying — and are never exercised in tests.

use std::path::Path;

use pcw_agent::install::{task_xml, utf16_le_bom};

#[test]
fn task_xml_disables_battery_and_time_limits() {
    let x = task_xml(Path::new("C:\\Users\\K & Co\\pcw-agent.exe"), "PC\\Kartik");
    for s in [
        "<DisallowStartIfOnBatteries>false",
        "<StopIfGoingOnBatteries>false",
        "<ExecutionTimeLimit>PT0S",
        "<RunLevel>HighestAvailable",
        "<LogonType>InteractiveToken",
        "<Priority>5",
        "<MultipleInstancesPolicy>IgnoreNew",
    ] {
        assert!(x.contains(s), "{s}");
    }
    assert!(x.contains("K &amp; Co"));
    assert!(!x.contains("K & Co"));
}

#[test]
fn utf16_file_has_bom() {
    let b = utf16_le_bom("<a/>");
    assert_eq!(&b[..2], &[0xFF, 0xFE]);
    assert_eq!(b.len(), 2 + 4 * 2);
}
