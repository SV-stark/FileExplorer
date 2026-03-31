use crate::models::search_engine_config::SearchEngineConfig;
use crate::search_engine::search_core::{EngineStats, SearchCore};
use crate::state::SettingsState;
#[allow(unused_imports)]
use crate::{log_error, log_info, log_warn};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant};
use std::{fs};
use tokio;



/// Current operational status of the search engine.
///
/// Represents the various states the search engine can be in at any given time,
/// allowing the UI to update accordingly and prevent conflicting operations.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub enum SearchEngineStatus {
    Idle,
    Indexing,
    Searching,
    Cancelled,
    Failed,
}

/// Progress information for ongoing indexing operations.
///
/// Tracks the current state of an indexing operation, including completion percentage
/// and estimated time remaining, to provide feedback for the user interface.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct IndexingProgress {
    pub files_discovered: usize,
    pub files_indexed: usize,
    pub percentage_complete: f32,
    pub current_path: Option<String>,
    pub start_time: Option<u64>, // as milliseconds since epoch
    pub estimated_time_remaining: Option<u64>, // in milliseconds
}

impl Default for IndexingProgress {
    fn default() -> Self {
        Self {
            files_discovered: 0,
            files_indexed: 0,
            percentage_complete: 0.0,
            current_path: None,
            start_time: None,
            estimated_time_remaining: None,
        }
    }
}

/// Performance metrics for the search engine.
///
/// Collects statistics about search engine performance to help users
/// understand system behavior and identify potential optimizations.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SearchEngineMetrics {
    pub last_indexing_duration_ms: Option<u64>,
    pub average_search_time_ms: Option<f32>,
    pub cache_hit_rate: Option<f32>,
    pub total_searches: usize,
    pub cache_hits: usize,
}

impl Default for SearchEngineMetrics {
    fn default() -> Self {
        Self {
            last_indexing_duration_ms: None,
            average_search_time_ms: None,
            cache_hit_rate: None,
            total_searches: 0,
            cache_hits: 0,
        }
    }
}

/// User activity data related to search operations.
///
/// Tracks recent user interactions with the search system to provide
/// history features and improve result relevance through usage patterns.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RecentActivity {
    pub recent_searches: Vec<String>,
    pub most_accessed_paths: Vec<String>,
}

impl Default for RecentActivity {
    fn default() -> Self {
        Self {
            recent_searches: Vec::new(),
            most_accessed_paths: Vec::new(),
        }
    }
}

/// Serializable version of engine statistics.
///
/// Provides a Serde-compatible representation of internal engine statistics
/// for transmission to the frontend or storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatsSerializable {
    pub cache_size: usize,
    pub trie_size: usize,
}

impl From<EngineStats> for EngineStatsSerializable {
    fn from(stats: EngineStats) -> Self {
        Self {
            cache_size: stats.cache_size,
            trie_size: stats.trie_size,
        }
    }
}

/// Comprehensive information about the search engine's current state.
///
/// Aggregates all relevant status information, metrics, and activity data
/// into a single serializable structure for frontend display and monitoring.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SearchEngineInfo {
    pub status: SearchEngineStatus,
    pub progress: IndexingProgress,
    pub metrics: SearchEngineMetrics,
    pub recent_activity: RecentActivity,
    pub stats: EngineStatsSerializable,
    pub last_updated: u64,
}

/// Complete search engine state including both configuration and runtime data.
///
/// Contains all persistent configuration options and runtime state of the
/// search engine system for storage and restoration between sessions.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SearchEngine {
    pub status: SearchEngineStatus,
    pub index_folder: PathBuf,
    pub progress: IndexingProgress,
    pub metrics: SearchEngineMetrics,
    pub config: SearchEngineConfig,
    pub recent_activity: RecentActivity,
    pub current_directory: Option<String>,
    pub last_updated: u64, // timestamp in milliseconds
}

impl Default for SearchEngine {
    fn default() -> Self {
        SearchEngine {
            status: SearchEngineStatus::Idle,
            index_folder: PathBuf::new(),
            progress: IndexingProgress::default(),
            metrics: SearchEngineMetrics::default(),
            config: SearchEngineConfig::default(),
            recent_activity: RecentActivity::default(),
            current_directory: None,
            last_updated: jiff::Timestamp::now().as_millisecond() as u64,
        }
    }
}

/// Thread-safe container for search engine state and operations.
///
/// Provides synchronized access to the search engine's configuration, state,
/// and underlying search index through a mutex-protected interface.
/// Offers methods for searching, indexing, and managing the search engine.
pub struct SearchEngineState {
    pub data: Arc<Mutex<SearchEngine>>,
    pub engine: Arc<RwLock<SearchCore>>,
    settings_state: Arc<Mutex<SettingsState>>,
}

impl SearchEngineState {
    /// Creates a new SearchEngineState with default settings.
    pub fn new(settings_state: Arc<Mutex<SettingsState>>) -> Self {
        // Get config from settings_state
        let config = {
            let settings = settings_state.lock().expect("Failed to acquire lock on settings state during SearchEngineState initialization");
            let inner_settings = settings.0.lock().expect("Failed to acquire lock on inner settings during SearchEngineState initialization");
            inner_settings.backend_settings.search_engine_config.clone()
        };

        // Create a new RankingConfig with the directory boost enabled/disabled
        // based on the prefer_directories setting
        let mut ranking_config = config.ranking_config.clone();
        if !config.prefer_directories {
            ranking_config.directory_ranking_boost = 0.0; // Disable directory boost if not preferred
        }

        // Pass the ranking_config from settings to the autocomplete engine
        let engine = SearchCore::new(
            config.cache_size,
            config.max_results,
            config.cache_ttl.unwrap_or_else(|| std::time::Duration::from_secs(3600)),  // Default 1 hour TTL
            ranking_config,
        );

        Self {
            data: Arc::new(Mutex::new(Self::save_default_search_engine_in_state(
                config,
            ))),
            engine: Arc::new(RwLock::new(engine)),
            settings_state,
        }
    }

    fn save_default_search_engine_in_state(config: SearchEngineConfig) -> SearchEngine {
        let mut defaults = SearchEngine::default();
        defaults.config = config;
        defaults
    }

    /// Starts indexing a folder for searching.
    #[allow(dead_code)]
    pub fn start_indexing(&self, folder: PathBuf) -> Result<(), String> {
        // Get locks on both data and engine
        let mut data = self.data.lock().map_err(|_| "Failed to lock search engine data")?;
        let mut engine = self.engine.write().map_err(|_| "Failed to acquire write lock on search engine")?;

        // Check if search engine is enabled
        if !data.config.search_engine_enabled {
            log_error!("Search engine is disabled in configuration.");
            return Err("Search engine is disabled in configuration".to_string());
        }

        // Check if we're already indexing - if so, stop it first
        if matches!(data.status, SearchEngineStatus::Indexing) {
            engine.stop_indexing();
        }

        // Update state to show we're indexing a new folder
        data.status = SearchEngineStatus::Indexing;
        data.index_folder = folder.clone();
        data.progress = IndexingProgress::default();
        data.progress.start_time = Some(jiff::Timestamp::now().as_millisecond() as u64);
        data.last_updated = jiff::Timestamp::now().as_millisecond() as u64;

        // Reset the stop flag before starting new indexing
        engine.reset_stop_flag();

        // Start indexing in the engine
        let start_time = Instant::now();

        // Clear previous index if switching folders
        engine.clear();

        // Get excluded patterns from config
        let excluded_patterns = data.config.excluded_patterns.clone();

        // Actually start the indexing
        if let Some(folder_str) = folder.to_str() {
            // Release the locks before starting the recursive operation
            drop(data);
            drop(engine);

            // Get the engine again for the recursive operation
            {
                let mut engine = self.engine.write().map_err(|_| "Failed to acquire write lock on search engine")?;
                // Since add_paths_recursive is async, we need to use a runtime
                let rt = tokio::runtime::Runtime::new().map_err(|_| "Failed to create tokio runtime")?;
                let patterns = excluded_patterns.as_ref();
                rt.block_on(engine.add_paths_recursive(folder_str, patterns));
            }

            // Update status and metrics after indexing completes or stops
            let mut data = self.data.lock().map_err(|_| "Failed to lock search engine data")?;
            let elapsed = start_time.elapsed();
            data.metrics.last_indexing_duration_ms = Some(elapsed.as_millis() as u64);

            // Check if it was cancelled
            let engine = self.engine.read().map_err(|_| "Failed to acquire read lock on search engine")?;
            if engine.should_stop_indexing() {
                data.status = SearchEngineStatus::Cancelled;
            } else {
                data.status = SearchEngineStatus::Idle;
            }
        } else {
            data.status = SearchEngineStatus::Failed;
            return Err("Invalid folder path".to_string());
        }

        Ok(())
    }

    /// Starts indexing a folder in chunks to prevent crashes with large directories.
    pub fn start_chunked_indexing(&self, folder: PathBuf, chunk_size: usize) -> Result<(), String> {
        // Get locks on both data and engine
        let mut data = self.data.lock().map_err(|_| "Failed to lock search engine data")?;
        let mut engine = self.engine.write().map_err(|_| "Failed to acquire write lock on search engine")?;

        // Check if search engine is enabled
        if !data.config.search_engine_enabled {
            log_error!("Search engine is disabled in configuration.");
            return Err("Search engine is disabled in configuration".to_string());
        }

        // Check if we're already indexing - if so, stop it first
        if matches!(data.status, SearchEngineStatus::Indexing) {
            engine.stop_indexing();
        }

        // Update state to show we're indexing a new folder
        data.status = SearchEngineStatus::Indexing;
        data.index_folder = folder.clone();
        data.progress = IndexingProgress::default();
        data.progress.start_time = Some(jiff::Timestamp::now().as_millisecond() as u64);
        data.last_updated = jiff::Timestamp::now().as_millisecond() as u64;

        // Reset the stop flag before starting new indexing
        engine.reset_stop_flag();

        // Start indexing in the engine
        engine.clear();

        // Get excluded patterns from config
        let excluded_patterns = data.config.excluded_patterns.clone();

        // Actually start the chunked indexing
        if let Some(_folder_str) = folder.to_str() {
            // Release the locks before starting the recursive operation
            drop(data);
            drop(engine);

            // Initialize progress tracking with immediate update
            {
                let mut data = self.data.lock().map_err(|_| "Failed to lock search engine data for progress update")?;
                data.progress.files_discovered = 0;
                data.progress.files_indexed = 0;
                data.progress.percentage_complete = 0.0;
                data.progress.current_path = Some(folder.to_string_lossy().to_string());
                data.last_updated = jiff::Timestamp::now().as_millisecond() as u64;
            }

            // Use streaming indexing instead of collecting all paths first
            let patterns = excluded_patterns.unwrap_or_default();
            self.index_directory_streaming(&folder, &patterns, chunk_size)?;
        } else {
            data.status = SearchEngineStatus::Failed;
            return Err("Invalid folder path".to_string());
        }

        Ok(())
    }

    fn index_directory_streaming(
        &self,
        dir: &PathBuf,
        excluded_patterns: &[String],
        chunk_size: usize,
    ) -> Result<(), String> {
        let mut discovered_files = 0;
        let mut indexed_files = 0;
        let mut current_batch = Vec::with_capacity(chunk_size);
        let start_time = Instant::now();

        // Use iterative directory processing to prevent stack overflow
        self.process_directory_iterative(
            dir,
            excluded_patterns,
            &mut discovered_files,
            &mut indexed_files,
            &mut current_batch,
            chunk_size,
        )?;

        // Process any remaining files in the batch
        if !current_batch.is_empty() {
            self.process_batch(&current_batch, &mut indexed_files, discovered_files)?;
        }

        // Update final status
        let mut data = self.data.lock().map_err(|_| "Failed to lock search engine data for final status update")?;
        let elapsed = start_time.elapsed();
        data.metrics.last_indexing_duration_ms = Some(elapsed.as_millis() as u64);

        // Check if it was cancelled
        let engine = self.engine.read().map_err(|_| "Failed to acquire read lock on search engine for status check")?;
        if engine.should_stop_indexing() {
            data.status = SearchEngineStatus::Cancelled;
        } else {
            data.status = SearchEngineStatus::Idle;
            data.progress.files_indexed = indexed_files;
            data.progress.files_discovered = discovered_files;
            data.progress.percentage_complete = 100.0;
            data.progress.current_path = None;
            data.last_updated = jiff::Timestamp::now().as_millisecond() as u64;
        }

        Ok(())
    }

    fn process_directory_iterative(
        &self,
        root_dir: &PathBuf,
        excluded_patterns: &[String],
        discovered_files: &mut usize,
        indexed_files: &mut usize,
        current_batch: &mut Vec<String>,
        chunk_size: usize,
    ) -> Result<(), String> {
        use std::collections::VecDeque;
        
        // Use a queue for iterative traversal instead of recursion
        let mut dir_queue = VecDeque::new();
        dir_queue.push_back((root_dir.clone(), 0));
        
        const MAX_DEPTH: usize = 25;
        const MAX_FILES: usize = 500000;
        
        while let Some((current_dir, depth)) = dir_queue.pop_front() {
            if depth >= MAX_DEPTH || *discovered_files > MAX_FILES {
                continue;
            }

            // Check for cancellation
            {
                let engine = self.engine.read().map_err(|_| "Failed to acquire read lock on search engine")?;
                if engine.should_stop_indexing() {
                    return Ok(());
                }
            }

            // Process current directory
            let walker = jwalk::WalkDir::new(&current_dir)
                .follow_links(false)
                .skip_hidden(false)
                .max_depth(1);
            
            for entry_res in walker {
                if let Ok(entry) = entry_res {
                    // Check for cancellation on each entry
                    {
                        let engine = self.engine.read().map_err(|_| "Failed to acquire read lock")?;
                        if engine.should_stop_indexing() {
                            return Ok(());
                        }
                    }

                    let path = entry.path();

                    if let Some(path_str) = path.to_str() {
                        let should_exclude = excluded_patterns.iter().any(|pattern| {
                            path_str.contains(pattern)
                                || path_str.ends_with(pattern)
                                || path
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .map(|name: &str| name.contains(pattern))
                                    .unwrap_or(false)
                        });

                        if !should_exclude {
                            current_batch.push(path_str.to_string());
                            *discovered_files += 1;

                            if *discovered_files % 10 == 0 || *discovered_files == 1 {
                                self.update_progress_safely(*discovered_files, *indexed_files, Some(path_str.to_string()));
                            }

                            if current_batch.len() >= chunk_size {
                                self.process_batch(current_batch, indexed_files, *discovered_files)?;
                                current_batch.clear();
                                current_batch.reserve(chunk_size);
                            }

                            if entry.file_type.is_dir() {
                                dir_queue.push_back((path, depth + 1));
                            }
                        }
                    }
                }
            }

            if depth % 10 == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        Ok(())
    }

    fn update_progress_safely(&self, discovered: usize, indexed: usize, current_path: Option<String>) {
        if let Ok(mut data) = self.data.try_lock() {
            data.progress.files_discovered = discovered;
            data.progress.files_indexed = indexed;
            data.progress.current_path = current_path;
            
            if discovered > 0 {
                let base_percentage = (indexed as f32 / discovered as f32) * 85.0;
                let discovery_percentage = (discovered as f32 / (discovered as f32 + 50.0)) * 15.0;
                data.progress.percentage_complete = base_percentage + discovery_percentage;
            }
            
            data.last_updated = jiff::Timestamp::now().as_millisecond() as u64;
        }
    }

    fn process_batch(
        &self,
        batch: &[String],
        indexed_files: &mut usize,
        total_discovered: usize,
    ) -> Result<(), String> {
        if batch.is_empty() {
            return Ok(());
        }

        const SUB_BATCH_SIZE: usize = 25;
        
        for chunk in batch.chunks(SUB_BATCH_SIZE) {
            {
                let engine = self.engine.read().map_err(|_| "Failed lock")?;
                if engine.should_stop_indexing() {
                    return Ok(());
                }
            }

            {
                let mut engine = self.engine.write().map_err(|_| "Failed write lock")?;
                let batch_refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
                engine.add_paths_batch(batch_refs, None);
            }

            *indexed_files += chunk.len();

            if let Ok(mut data) = self.data.try_lock() {
                data.progress.files_indexed = *indexed_files;
                data.progress.files_discovered = total_discovered;
                
                data.progress.percentage_complete = if total_discovered > 0 {
                    (*indexed_files as f32 / total_discovered as f32) * 100.0
                } else {
                    0.0
                };

                if let Some(start_time_ms) = data.progress.start_time {
                    let elapsed_ms = jiff::Timestamp::now().as_millisecond() as u64 - start_time_ms;
                    if *indexed_files > 0 {
                        let avg_time_per_file = elapsed_ms as f32 / *indexed_files as f32;
                        let remaining_files = total_discovered.saturating_sub(*indexed_files);
                        let estimated_ms = (avg_time_per_file * remaining_files as f32) as u64;
                        data.progress.estimated_time_remaining = Some(estimated_ms);
                    }
                }

                data.last_updated = jiff::Timestamp::now().as_millisecond() as u64;
            }

            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        Ok(())
    }

    pub fn search(&self, query: &str) -> Result<Vec<(String, f32)>, String> {
        {
            let mut data = self.data.lock().map_err(|_| "Failed to lock search engine data for search operation")?;

            if !data.config.search_engine_enabled {
                return Err("Search engine is disabled in configuration".to_string());
            }

            if matches!(data.status, SearchEngineStatus::Indexing) {
                return Err("Engine is currently indexing".to_string());
            }

            data.status = SearchEngineStatus::Searching;
            data.last_updated = jiff::Timestamp::now().as_millisecond() as u64;
        }

        let results = {
            let mut engine = self.engine.write().map_err(|_| "Failed to acquire write lock on search engine")?;
            
            let start_time = Instant::now();
            let results = engine.search(query);
            let search_time = start_time.elapsed();
            let was_cache_hit = engine.was_last_search_cache_hit();
            (results, search_time, was_cache_hit)
        };

        let (search_results, search_time, was_cache_hit) = results;

        if let Ok(mut data) = self.data.lock() {
            data.metrics.total_searches += 1;
            if was_cache_hit {
                data.metrics.cache_hits += 1;
            }

            if data.metrics.total_searches > 0 {
                let hit_rate = (data.metrics.cache_hits as f32 / data.metrics.total_searches as f32) * 100.0;
                data.metrics.cache_hit_rate = Some(hit_rate.min(100.0).max(0.0));
            }

            if let Some(avg_time) = data.metrics.average_search_time_ms {
                data.metrics.average_search_time_ms = Some(
                    (avg_time * (data.metrics.total_searches - 1) as f32
                        + search_time.as_millis() as f32)
                        / data.metrics.total_searches as f32,
                );
            } else {
                data.metrics.average_search_time_ms = Some(search_time.as_millis() as f32);
            }

            if !query.is_empty() {
                data.recent_activity.recent_searches.insert(0, query.to_string());
                if data.recent_activity.recent_searches.len() > 10 {
                    data.recent_activity.recent_searches.pop();
                }
            }

            data.status = SearchEngineStatus::Idle;
        }

        Ok(search_results)
    }

    pub fn search_by_extension(
        &self,
        query: &str,
        extensions: Vec<String>,
    ) -> Result<Vec<(String, f32)>, String> {
        {
            let mut data = self.data.lock().map_err(|_| "Failed to lock search engine data")?;

            if !data.config.search_engine_enabled {
                return Err("Search engine is disabled in configuration".to_string());
            }

            if matches!(data.status, SearchEngineStatus::Indexing) {
                return Err("Engine is currently indexing".to_string());
            }

            data.status = SearchEngineStatus::Searching;
            data.last_updated = jiff::Timestamp::now().as_millisecond() as u64;
        }

        let results = {
            let mut engine = self.engine.write().map_err(|_| "Failed to acquire write lock")?;
            let original_extensions = engine.get_preferred_extensions().clone();
            engine.set_preferred_extensions(extensions);

            let start_time = Instant::now();
            let results = engine.search(query);
            let search_time = start_time.elapsed();
            let was_cache_hit = engine.was_last_search_cache_hit();

            engine.set_preferred_extensions(original_extensions);
            (results, search_time, was_cache_hit)
        };

        let (search_results, search_time, was_cache_hit) = results;

        if let Ok(mut data) = self.data.lock() {
            data.metrics.total_searches += 1;
            if was_cache_hit {
                data.metrics.cache_hits += 1;
            }

            if data.metrics.total_searches > 0 {
                let hit_rate = (data.metrics.cache_hits as f32 / data.metrics.total_searches as f32) * 100.0;
                data.metrics.cache_hit_rate = Some(hit_rate.min(100.0).max(0.0));
            }

            if let Some(avg_time) = data.metrics.average_search_time_ms {
                data.metrics.average_search_time_ms = Some(
                    (avg_time * (data.metrics.total_searches - 1) as f32
                        + search_time.as_millis() as f32)
                        / data.metrics.total_searches as f32,
                );
            } else {
                data.metrics.average_search_time_ms = Some(search_time.as_millis() as f32);
            }

            data.status = SearchEngineStatus::Idle;
        }

        Ok(search_results)
    }

    #[cfg(test)]
    pub fn update_indexing_progress(
        &self,
        indexed: usize,
        total: usize,
        current_path: Option<String>,
    ) {
        if let Ok(mut data) = self.data.lock() {
            data.progress.files_indexed = indexed;
            data.progress.files_discovered = total;
            data.progress.current_path = current_path;

            if total > 0 {
                data.progress.percentage_complete = (indexed as f32 / total as f32) * 100.0;
            }

            if let Some(start_time) = data.progress.start_time {
                let elapsed_ms = jiff::Timestamp::now().as_millisecond() as u64 - start_time;
                if indexed > 0 {
                    let avg_time_per_file = elapsed_ms as f32 / indexed as f32;
                    let remaining_files = total.saturating_sub(indexed);
                    let estimated_ms = (avg_time_per_file * remaining_files as f32) as u64;
                    data.progress.estimated_time_remaining = Some(estimated_ms);
                }
            }

            data.last_updated = jiff::Timestamp::now().as_millisecond() as u64;
        }
    }

    pub fn get_stats(&self) -> EngineStatsSerializable {
        let engine = self.engine.read().expect("Failed to acquire read lock");
        EngineStatsSerializable::from(engine.get_stats())
    }

    pub fn get_search_engine_info(&self) -> SearchEngineInfo {
        let data = self.data.lock().expect("Failed lock");
        let stats = self.get_stats();
        SearchEngineInfo {
            status: data.status.clone(),
            progress: data.progress.clone(),
            metrics: data.metrics.clone(),
            recent_activity: data.recent_activity.clone(),
            stats,
            last_updated: data.last_updated,
        }
    }

    #[cfg(test)]
    pub fn update_config(&self, path: Option<String>) -> Result<(), String> {
        let mut data = self.data.lock().map_err(|_| "Failed lock")?;
        let config = {
            let settings = self.settings_state.lock().map_err(|_| "Failed lock")?;
            let inner_settings = settings.0.lock().map_err(|_| "Failed lock")?;
            inner_settings.backend_settings.search_engine_config.clone()
        };

        data.config = config.clone();
        data.last_updated = jiff::Timestamp::now().as_millisecond() as u64;
        data.current_directory = path;
        
        drop(data);

        let mut engine = self.engine.write().map_err(|_| "Failed lock")?;
        engine.set_preferred_extensions(config.preferred_extensions);
        Ok(())
    }

    pub fn add_path(&self, path: &str) -> Result<(), String> {
        let data = self.data.lock().map_err(|_| "Failed lock")?;
        if !data.config.search_engine_enabled {
            return Err("Search engine is disabled".to_string());
        }
        let excluded_patterns = data.config.excluded_patterns.clone();
        drop(data);

        let mut engine = self.engine.write().map_err(|_| "Failed lock")?;
        engine.add_path_with_exclusion_check(path, excluded_patterns.as_ref());
        Ok(())
    }

    pub fn remove_path(&self, path: &str) -> Result<(), String> {
        let data = self.data.lock().map_err(|_| "Failed lock")?;
        if !data.config.search_engine_enabled {
            return Err("Search engine is disabled".to_string());
        }
        drop(data);

        let mut engine = self.engine.write().map_err(|_| "Failed lock")?;
        engine.remove_path(path);
        Ok(())
    }

    pub fn remove_paths_recursive(&self, path: &str) -> Result<(), String> {
        let data = self.data.lock().map_err(|_| "Failed lock")?;
        if !data.config.search_engine_enabled {
            return Err("Search engine is disabled".to_string());
        }
        drop(data);

        let mut engine = self.engine.write().map_err(|_| "Failed lock")?;
        engine.remove_paths_recursive(path);
        Ok(())
    }

    #[cfg(test)]
    pub fn stop_indexing(&self) -> Result<(), String> {
        let mut data = self.data.lock().map_err(|_| "Failed lock")?;

        if matches!(data.status, SearchEngineStatus::Indexing) {
            data.status = SearchEngineStatus::Cancelled;
            data.last_updated = jiff::Timestamp::now().as_millisecond() as u64;
            drop(data);

            let mut engine = self.engine.write().map_err(|_| "Failed lock")?;
            engine.stop_indexing();
            return Ok(());
        }
        Err("No indexing operation in progress".to_string())
    }

    #[cfg(test)]
    pub fn cancel_indexing(&self) -> Result<(), String> {
        self.stop_indexing()
    }
}

impl Clone for SearchEngineState {
    fn clone(&self) -> Self {
        Self {
            data: Arc::clone(&self.data),
            engine: Arc::clone(&self.engine),
            settings_state: Arc::clone(&self.settings_state),
        }
    }
}

#[cfg(test)]
pub(crate) fn get_test_data_path() -> PathBuf {
    use crate::search_engine::test_generate_test_data::generate_test_data_if_not_exists;
    use crate::constants::TEST_DATA_PATH;

    let path = PathBuf::from(TEST_DATA_PATH);
    let _ = generate_test_data_if_not_exists(PathBuf::from(TEST_DATA_PATH));
    path
}

#[cfg(test)]
pub(crate) fn collect_test_paths(limit: Option<usize>) -> Vec<String> {
    let test_path = get_test_data_path();
    let mut paths = Vec::new();

    fn add_paths_recursively(dir: &std::path::Path, paths: &mut Vec<String>, limit: Option<usize>) {
        if let Some(max) = limit {
            if paths.len() >= max {
                return;
            }
        }

        if let Ok(walker) = fs::read_dir(dir) {
            for entry in walker.filter_map(|e| e.ok()) {
                let path = entry.path();
                if let Some(path_str) = path.to_str() {
                    paths.push(path_str.to_string());
                    if let Some(max) = limit {
                        if paths.len() >= max {
                            return;
                        }
                    }
                }
                if path.is_dir() {
                    add_paths_recursively(&path, paths, limit);
                }
            }
        }
    }

    add_paths_recursively(&test_path, &mut paths, limit);
    if paths.is_empty() {
        return (0..100).map(|i| format!("/path/to/file{}.txt", i)).collect();
    }
    paths
}

#[cfg(test)]
pub(crate) fn get_test_dir_for_indexing() -> PathBuf {
    let paths = collect_test_paths(Some(20));
    for path in &paths {
        let path_buf = PathBuf::from(path);
        if path_buf.is_dir() {
            return path_buf;
        }
    }
    get_test_data_path()
}

#[cfg(test)]
mod tests_searchengine_state {
    use super::*;
    use std::fs;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_initialization() {
        let settings_state = Arc::new(Mutex::new(SettingsState::new()));
        let state = SearchEngineState::new(settings_state);
        let data = state.data.lock().unwrap();
        assert_eq!(data.status, SearchEngineStatus::Idle);
    }

    #[cfg(feature = "long-tests")]
    #[test]
    fn test_start_indexing() {
        let settings_state = Arc::new(Mutex::new(SettingsState::new()));
        let state = SearchEngineState::new(settings_state);
        let test_dir = get_test_dir_for_indexing();
        let _ = state.start_indexing(test_dir.clone());
        thread::sleep(Duration::from_millis(200));
        let data = state.data.lock().unwrap();
        assert!(matches!(data.status, SearchEngineStatus::Idle | SearchEngineStatus::Cancelled));
    }

    #[cfg(feature = "long-tests")]
    #[test]
    fn test_stop_indexing() {
        let settings_state = Arc::new(Mutex::new(SettingsState::new()));
        let state = Arc::new(SearchEngineState::new(settings_state));
        let test_dir = get_test_dir_for_indexing();

        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let state_clone = Arc::clone(&state);
        let test_dir_clone = test_dir.clone();

        let indexing_thread = thread::spawn(move || {
            {
                let mut data = state_clone.data.lock().unwrap();
                data.status = SearchEngineStatus::Indexing;
                status_tx.send(()).unwrap();
            }
            let _ = state_clone.start_indexing(test_dir_clone);
        });

        status_rx.recv().unwrap();
        let _ = state.stop_indexing();
        indexing_thread.join().unwrap();
        let data = state.data.lock().unwrap();
        assert!(matches!(data.status, SearchEngineStatus::Cancelled | SearchEngineStatus::Idle));
    }

    #[test]
    fn test_cancel_indexing() {
        let settings_state = Arc::new(Mutex::new(SettingsState::new()));
        let state = Arc::new(SearchEngineState::new(settings_state));
        let test_dir = get_test_dir_for_indexing();

        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let state_clone = Arc::clone(&state);
        let test_dir_clone = test_dir.clone();

        let indexing_thread = thread::spawn(move || {
            {
                let mut data = state_clone.data.lock().unwrap();
                data.status = SearchEngineStatus::Indexing;
                status_tx.send(()).unwrap();
            }
            let _ = state_clone.start_indexing(test_dir_clone);
        });

        status_rx.recv().unwrap();
        let _ = state.cancel_indexing();
        indexing_thread.join().unwrap();
        let data = state.data.lock().unwrap();
        assert!(matches!(data.status, SearchEngineStatus::Cancelled | SearchEngineStatus::Idle));
    }

    #[test]
    fn test_search() {
        let settings_state = Arc::new(Mutex::new(SettingsState::new()));
        let state = SearchEngineState::new(settings_state);
        let _ = state.add_path("/test/file.txt");
        let result = state.search("file");
        assert!(result.is_ok());
    }

    #[test]
    fn test_directory_context_for_search() {
        let settings_state = Arc::new(Mutex::new(SettingsState::new()));
        let state = SearchEngineState::new(settings_state);
        let _ = state.add_path("/test/dir/file.txt");
        let _ = state.update_config(Some("/test/dir".to_string()));
        let result = state.search("file");
        assert!(result.is_ok());
    }

    #[test]
    fn test_add_and_remove_path() {
        let settings_state = Arc::new(Mutex::new(SettingsState::new()));
        let state = SearchEngineState::new(settings_state);
        let path = "/test/to/remove.txt";
        let _ = state.add_path(path);
        let _ = state.remove_path(path);
        let results = state.search("remove").unwrap();
        assert!(!results.iter().any(|(p, _)| p == path));
    }

    #[test]
    fn test_chunked_indexing_stop() {
        let settings_state = Arc::new(Mutex::new(SettingsState::new()));
        let state = Arc::new(SearchEngineState::new(settings_state));
        let test_dir = get_test_dir_for_indexing();

        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let state_clone = Arc::clone(&state);
        let test_dir_clone = test_dir.clone();

        let indexing_thread = thread::spawn(move || {
            {
                let mut data = state_clone.data.lock().unwrap();
                data.status = SearchEngineStatus::Indexing;
                status_tx.send(()).unwrap();
            }
            let _ = state_clone.start_chunked_indexing(test_dir_clone, 10);
        });

        status_rx.recv().unwrap();
        let _ = state.stop_indexing();
        indexing_thread.join().unwrap();
        let data = state.data.lock().unwrap();
        assert!(matches!(data.status, SearchEngineStatus::Cancelled | SearchEngineStatus::Idle));
    }
}

#[cfg(test)]
mod bench_indexing_methods {
    use super::*;
    use std::time::Instant;
    use std::thread;

    #[test]
    fn benchmark_indexing_methods_comparison() {
        let settings_state = Arc::new(Mutex::new(SettingsState::new()));
        let state = SearchEngineState::new(settings_state);
        let test_dir = get_test_dir_for_indexing();
        let _ = state.start_indexing(test_dir.clone());
        let _ = state.start_chunked_indexing(test_dir, 100);
    }

    #[test]
    fn benchmark_memory_usage_comparison() {
        let settings_state = Arc::new(Mutex::new(SettingsState::new()));
        let state = SearchEngineState::new(settings_state);
        let test_dir = get_test_dir_for_indexing();
        let _ = state.start_indexing(test_dir.clone());
        let _ = state.start_chunked_indexing(test_dir, 50);
    }

    #[test]
    fn test_concurrent_search_optimization() {
        let settings_state = Arc::new(Mutex::new(SettingsState::new()));
        let state = Arc::new(SearchEngineState::new(settings_state));
        
        let mut handles = Vec::new();
        for _ in 0..5 {
            let state_clone = Arc::clone(&state);
            handles.push(thread::spawn(move || {
                state_clone.search("file")
            }));
        }

        for handle in handles {
            let _ = handle.join().unwrap();
        }
    }
}
