use crate::scan::{self, CCScanResult};
use std::path::{Path, PathBuf};

// ============================================================
// Phase 2.1-2.2: CCWatcher struct + constructor
// ============================================================

/// Watches the CC binary for updates and triggers re-scans
#[allow(dead_code)]
pub struct CCWatcher {
    binary_path: PathBuf,
    last_sha256: String,
    last_scan: Option<CCScanResult>,
    state_path: PathBuf,
}

#[allow(dead_code)]
impl CCWatcher {
    /// Create a new watcher with an initial scan result
    pub fn new(
        binary_path: PathBuf,
        initial_sha256: String,
        initial_scan: Option<CCScanResult>,
    ) -> Self {
        let state_path = dirs_next_home()
            .map(|h| h.join(".nexus-ai-gateway-scan.json"))
            .unwrap_or_else(|| PathBuf::from("/tmp/nexus-ai-gateway-scan.json"));

        CCWatcher {
            binary_path,
            last_sha256: initial_sha256,
            last_scan: initial_scan,
            state_path,
        }
    }

    // ============================================================
    // Phase 2.3: check_for_update()
    // ============================================================

    /// Check if the CC binary has been updated since last scan
    pub fn check_for_update(&mut self) -> bool {
        let current_sha256 = match scan::compute_sha256(&self.binary_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("⚠️ Cannot compute CC binary SHA256: {}", e);
                return false;
            }
        };

        if current_sha256 != self.last_sha256 {
            tracing::info!(
                "⚠️ CC binary updated!\n   Old: {}...{}\n   New: {}...{}",
                &self.last_sha256[..8],
                &self.last_sha256[self.last_sha256.len() - 8..],
                &current_sha256[..8],
                &current_sha256[current_sha256.len() - 8..]
            );

            // Re-scan
            match scan::scan_cc_binary() {
                Ok(new_scan) => {
                    // Compare with old scan
                    if let Some(old) = &self.last_scan {
                        let new_models: Vec<&str> = new_scan
                            .models
                            .iter()
                            .filter(|m| !old.models.iter().any(|o| o.id == m.id))
                            .map(|m| m.id.as_str())
                            .collect();
                        let new_tools: Vec<&str> = new_scan
                            .tools
                            .iter()
                            .filter(|t| !old.tools.contains(t))
                            .map(|t| t.as_str())
                            .collect();

                        if !new_models.is_empty() {
                            tracing::info!("📊 NEW models: {:?}", new_models);
                        }
                        if !new_tools.is_empty() {
                            tracing::info!("📊 NEW tools: {:?}", new_tools);
                        }
                    }

                    self.last_sha256 = current_sha256;
                    self.last_scan = Some(new_scan);

                    // Save state
                    if let Err(e) = self.save_state() {
                        tracing::warn!("⚠️ Cannot save scan state: {}", e);
                    }

                    true
                }
                Err(e) => {
                    tracing::error!("❌ Re-scan failed: {}", e);
                    false
                }
            }
        } else {
            false
        }
    }

    // ============================================================
    // Phase 2.4: save_state() and load_state()
    // ============================================================

    /// Save the current scan result to disk
    pub fn save_state(&self) -> Result<(), String> {
        if let Some(scan) = &self.last_scan {
            let json = serde_json::to_string_pretty(scan)
                .map_err(|e| format!("Serialize error: {}", e))?;
            std::fs::write(&self.state_path, json).map_err(|e| format!("Write error: {}", e))?;
            tracing::debug!("💾 Scan state saved: {}", self.state_path.display());
        }
        Ok(())
    }

    /// Load a previous scan result from disk
    pub fn load_state(path: &Path) -> Option<CCScanResult> {
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Get the last scan result
    pub fn last_scan(&self) -> Option<&CCScanResult> {
        self.last_scan.as_ref()
    }
}

fn dirs_next_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}
