mod backend;
mod bridge;
mod demo;
mod proposals;
mod provider;
mod ui;
mod verification;

type ExampleResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1100.0, 780.0]),
        ..Default::default()
    };
    eframe::run_native(
        "AgentKit Native Reference",
        options,
        Box::new(|context| Ok(Box::new(ui::NativeApp::new(context)?))),
    )
}

#[cfg(test)]
mod tests;
