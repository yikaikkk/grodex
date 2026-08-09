use anyhow::Result;

fn main() -> Result<()> {
    let transport = grodex_tui::transport::in_process::InProcessBridge::new(16);
    let tui = grodex_tui::GrodexTui::init_with(transport)?;
    tui.run_blocking()
}
