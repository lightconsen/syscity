//! Syscity - Main entry point
//!
//! This is the main entry point for the Syscity application.
//! It initializes the application and runs the CLI.

use std::sync::Arc;

use syscity::cli::Cli;

#[tokio::main]
async fn main() {
    // Pin the process-wide path root explicitly, once, before anything reads it.
    // Resolving `SYSCITY_HOME` / `~/.syscity` here is what lets the rest of the
    // binary stop reaching for the environment: the CLI is a single-root
    // process, so one install covers every `dirs::` free-function call.
    if syscity::dirs::set_default_paths(Arc::new(syscity::dirs::SyscityPaths::from_env())).is_err()
    {
        // A root was already installed by an outer embedder — that one wins.
        tracing::debug!("path root already installed; keeping the existing one");
    }

    // Initialize the application
    if let Err(e) = syscity::init() {
        eprintln!("Failed to initialize: {}", e);
        std::process::exit(1);
    }

    // Run the CLI
    if let Err(e) = Cli::run().await {
        handle_error(e);
    }
}

/// Handle an error and exit the application
fn handle_error(error: syscity::error::SyscityError) {
    use syscity::error::SyscityError;

    let exit_code = match &error {
        SyscityError::Config(_) => 2,
        SyscityError::Validation(_) => 3,
        SyscityError::NotFound { .. } => 4,
        SyscityError::ExternalService { .. } => 5,
        _ => 1,
    };

    // Use different formatting based on the error type
    match &error {
        SyscityError::Validation(msg) => {
            eprintln!("❌ Validation error: {}", msg);
        }
        SyscityError::NotFound { resource } => {
            eprintln!("🔍 Not found: {}", resource);
        }
        SyscityError::Config(e) => {
            eprintln!("⚙️  Configuration error: {}", e);
        }
        SyscityError::Io(e) => {
            eprintln!("📁 I/O error: {}", e);
        }
        SyscityError::Http(e) => {
            eprintln!("🌐 HTTP error: {}", e);
        }
        SyscityError::ExternalService { source, .. } => {
            eprintln!("🔌 External service error: {}", source);
        }
        _ => {
            eprintln!("💥 Error: {}", error);
        }
    }

    // Add helpful hints for common errors
    if let SyscityError::Config(_) = &error {
        eprintln!();
        eprintln!("Hint: Check your configuration file or environment variables.");
        eprintln!("      Run 'syscity config' to see the current configuration.");
    }

    std::process::exit(exit_code);
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_version_available() {
        assert!(!syscity::VERSION.is_empty());
    }
}
