//! Explicit native surfaces keep the launcher opaque on both desktop platforms.
pub fn initialize() -> Result<(), slint::PlatformError> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_window_attributes_hook(|attributes| {
            let attributes = attributes
                .with_transparent(false)
                .with_decorations(true)
                .with_theme(Some(slint::winit_030::winit::window::Theme::Light));
            #[cfg(target_os = "macos")]
            {
                use slint::winit_030::winit::platform::macos::WindowAttributesExtMacOS;
                attributes
                    .with_titlebar_transparent(false)
                    .with_titlebar_hidden(false)
                    .with_title_hidden(false)
                    .with_fullsize_content_view(false)
            }
            #[cfg(not(target_os = "macos"))]
            attributes
        })
        .select()
}
