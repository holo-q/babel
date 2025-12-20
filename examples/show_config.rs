//! Example showing the default configuration structure

use claude_babel::config::BabelConfig;

fn main() {
    let config = BabelConfig::default();
    let toml = toml::to_string_pretty(&config).unwrap();

    println!("# Example babel.toml configuration");
    println!("# Place this at ~/.config/babel/babel.toml\n");
    println!("{}", toml);
}
