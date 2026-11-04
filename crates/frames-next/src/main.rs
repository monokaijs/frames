//! The `frames-next` binary; see the library for the commands.

fn main() -> anyhow::Result<()> {
    frames_next::main_from(std::env::args().skip(1).collect())
}
