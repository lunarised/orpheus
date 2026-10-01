use crate::config::{Config, NowPlayingStyle, PlaylistEntry, ScreensaverStyle, Settings};
use crate::history::HistoryEntry;
use crate::mopidy::{
    MopidyBrowseItem, MopidyLibraryClient, MopidyQueueTrack, MopidySearchTrack, QueuePlacement,
};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_ARTWORK_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LIBRARY_SEARCH_RESULTS: usize = 30;
// A prolific artist can legitimately have hundreds of Jellyfin tracks. Keep
// enough validated lookup results that the first albums in a large artist view
// remain queueable while still bounding the server-side allow-list.
const MAX_RECENT_SEARCH_URIS: usize = 4_096;
const SETTINGS_SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const LOGIN_FAILURE_WINDOW: Duration = Duration::from_secs(60);
const MAX_LOGIN_FAILURES: usize = 8;
type LibrarySearch =
    dyn Fn(&str, usize) -> Result<Vec<MopidySearchTrack>, String> + Send + Sync + 'static;
type LibraryBrowse =
    dyn Fn(&str, usize) -> Result<Vec<MopidyBrowseItem>, String> + Send + Sync + 'static;
type LibraryLookup =
    dyn Fn(&str, usize) -> Result<Vec<MopidySearchTrack>, String> + Send + Sync + 'static;
const IDENTITY_GATE_HTML: &str = r#"<div id="identity-gate" class="identity-gate" role="dialog" aria-modal="true" aria-labelledby="identity-title" hidden><section class="identity-card"><div class="brand-mark">O</div><div class="eyebrow">Welcome to Orpheus</div><h2 id="identity-title">What should we call you?</h2><p>Your name stays in this browser and labels songs you add to the shared queue.</p><form id="identity-form"><label>Display name<input id="identity-name" type="text" minlength="1" maxlength="32" autocomplete="nickname" placeholder="Your name" required></label><p id="identity-error" class="login-error" aria-live="polite"></p><button type="submit">Enter Orpheus</button></form></section></div>"#;

const IDENTITY_JS: &str = r#"(() => {
  const storageKey = 'orpheus-requester-name';
  const gate = document.getElementById('identity-gate');
  const form = document.getElementById('identity-form');
  const input = document.getElementById('identity-name');
  const error = document.getElementById('identity-error');
  const main = document.querySelector('main');
  const requester = document.getElementById('requester-name');
  const label = document.getElementById('client-name-label');
  const change = document.getElementById('change-client-name');
  if (!gate || !form || !input || !main) return;

  let currentName = '';
  const normalize = (value) => String(value || '').trim();
  const valid = (value) => {
    const length = Array.from(value).length;
    return length >= 1 && length <= 32 && !/[\u0000-\u001f\u007f-\u009f]/u.test(value);
  };
  const publish = (name) => {
    currentName = name;
    if (requester) requester.value = name;
    if (label) label.textContent = name;
  };
  const unlock = (name) => {
    publish(name);
    gate.hidden = true;
    main.removeAttribute('inert');
    document.body.classList.remove('identity-pending', 'identity-required');
  };
  const open = () => {
    input.value = currentName;
    if (error) error.textContent = '';
    gate.hidden = false;
    main.setAttribute('inert', '');
    document.body.classList.remove('identity-pending');
    document.body.classList.add('identity-required');
    window.requestAnimationFrame(() => input.focus());
  };

  let stored = '';
  try { stored = normalize(window.localStorage.getItem(storageKey)); } catch (_error) {}
  if (valid(stored)) unlock(stored); else open();

  form.addEventListener('submit', (event) => {
    event.preventDefault();
    const name = normalize(input.value);
    if (!valid(name)) {
      if (error) error.textContent = 'Enter 1 to 32 printable characters.';
      input.focus();
      return;
    }
    try { window.localStorage.setItem(storageKey, name); } catch (_error) {}
    unlock(name);
  });
  if (change) change.addEventListener('click', open);
})();"#;

const APP_JS: &str = r#"(() => {
  document.documentElement.classList.add('js');
  const get = (id) => document.getElementById(id);
  const guestMode = Boolean(get('guest-mode'));
  const status = get('playback-status');
  const title = get('track-title');
  const meta = get('track-meta');
  const toggle = get('toggle-playback');
  const progress = get('playback-progress');
  const progressBar = get('playback-progress-bar');
  const elapsed = get('elapsed-time');
  const remaining = get('remaining-time');
  const next = get('next-track');
  const screenToggle = get('toggle-screen');
  const carouselStatus = get('carousel-status');
  const volume = get('playback-volume');
  const volumeLabel = get('volume-label');
  const playbackForm = get('playback-form');
  const playbackEndpoint = playbackForm.getAttribute('action') || '/playback';
  const csrf = get('playback-csrf');
  const feedback = get('command-feedback');
  const artwork = get('album-art');
  const artworkWrap = get('album-art-wrap');
  const quickPlaylistForm = get('quick-playlist-form');
  const playlistFilter = get('playlist-filter');
  const playlistSelect = get('playlist-select');
  const playlistCount = get('playlist-count');
  const quickPlaylistPlay = get('quick-playlist-play');
  const quickPlaylistCsrf = get('quick-playlist-csrf');
  const librarySearchForm = get('library-search-form');
  const librarySearchInput = get('library-search-input');
  const librarySearchButton = get('library-search-button');
  const librarySearchStatus = get('library-search-status');
  const librarySearchResults = get('library-search-results');
  const librarySearchCsrf = get('library-search-csrf');
  const requesterName = get('requester-name');
  const libraryBrowseKind = get('library-browse-kind');
  const libraryBrowseFilter = get('library-browse-filter');
  const libraryBrowseItems = get('library-browse-items');
  const libraryBrowseTracks = get('library-browse-tracks');
  const libraryBrowseStatus = get('library-browse-status');
  const queueList = get('queue-list');
  const queueStatus = get('queue-status');
  const queueClear = get('queue-clear');
  let latestPlayback = {};
  let currentArtworkId = artwork.dataset.artworkId || null;
  let feedbackTimer;
  let volumeTimer;
  let pendingVolume = null;
  let volumeDragging = false;
  let volumeEditingUntil = 0;
  let playlistCatalog = [];
  let searchTracks = [];
  let searchTimer;
  let searchController;
  let browseItems = [];
  let browseTracks = [];

  const formatTime = (seconds) => {
    const total = Math.max(0, Math.floor(Number(seconds) || 0));
    const hours = Math.floor(total / 3600);
    const minutes = Math.floor((total % 3600) / 60);
    const secs = String(total % 60).padStart(2, '0');
    return hours > 0 ? `${hours}:${String(minutes).padStart(2, '0')}:${secs}` : `${minutes}:${secs}`;
  };

  const showFeedback = (message, isError = false) => {
    window.clearTimeout(feedbackTimer);
    feedback.textContent = message;
    feedback.classList.toggle('error', isError);
    feedbackTimer = window.setTimeout(() => { feedback.textContent = ''; }, 2500);
  };

  const setOnline = (online, mopidyOnline) => {
    document.querySelectorAll('[data-playback-control]').forEach((control) => {
      control.disabled = !online;
    });
    if (guestMode) document.querySelectorAll('[data-admin-control]').forEach((control) => {
      control.disabled = true;
    });
    document.querySelectorAll('[data-mopidy-control]').forEach((control) => {
      control.disabled = !mopidyOnline;
    });
  };

  const renderPlaylistOptions = () => {
    if (!playlistSelect) return;
    const query = (playlistFilter && playlistFilter.value || '').trim().toLocaleLowerCase();
    const previous = playlistSelect.value;
    const visible = playlistCatalog.filter((playlist) => !query || playlist.name.toLocaleLowerCase().includes(query));
    playlistSelect.replaceChildren();
    visible.forEach((playlist) => {
      const option = document.createElement('option');
      option.value = playlist.uri;
      option.textContent = `${playlist.favorite ? '★ ' : ''}${playlist.name}`;
      option.dataset.name = playlist.name;
      playlistSelect.append(option);
    });
    if (visible.some((playlist) => playlist.uri === previous)) playlistSelect.value = previous;
    if (playlistCount) playlistCount.textContent = query ? `${visible.length} of ${playlistCatalog.length}` : `${playlistCatalog.length} available`;
    if (quickPlaylistPlay) quickPlaylistPlay.disabled = !latestPlayback.mopidy_online || visible.length === 0;
  };

  const refreshPlaylists = async () => {
    if (!playlistSelect) return;
    try {
      const response = await fetch('/api/playlists', {cache: 'no-store'});
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const catalog = await response.json();
      if (!Array.isArray(catalog)) throw new Error('Invalid playlist response');
      playlistCatalog = catalog.filter((playlist) => playlist && playlist.name && playlist.uri);
      renderPlaylistOptions();
    } catch (_error) {
      if (playlistCount) playlistCount.textContent = 'Playlist refresh unavailable';
    }
  };

  const renderSearchResults = (tracks) => {
    searchTracks = Array.isArray(tracks) ? tracks : [];
    librarySearchResults.replaceChildren();
    searchTracks.forEach((track, index) => {
      const row = document.createElement('article');
      row.className = 'search-result';
      const copy = document.createElement('div');
      copy.className = 'search-result-copy';
      const heading = document.createElement('strong');
      heading.textContent = track.title || 'Unknown track';
      const details = document.createElement('span');
      details.textContent = [track.artist, track.album, formatTime((Number(track.duration_ms) || 0) / 1000)].filter(Boolean).join(' · ');
      copy.append(heading, details);
      const actions = document.createElement('div');
      actions.className = 'search-actions';
      const placements = guestMode ? [['end', 'Add']] : [['now', 'Play now'], ['next', 'Play next'], ['end', 'Add']];
      placements.forEach(([placement, label]) => {
        const button = document.createElement('button');
        button.type = 'button';
        button.textContent = label;
        button.dataset.searchIndex = String(index);
        button.dataset.placement = placement;
        button.dataset.mopidyControl = '';
        button.disabled = latestPlayback.mopidy_online === false;
        actions.append(button);
      });
      row.append(copy, actions);
      librarySearchResults.append(row);
    });
  };

  const renderBrowseItems = () => {
    if (!libraryBrowseItems) return;
    const query = (libraryBrowseFilter && libraryBrowseFilter.value || '').trim().toLocaleLowerCase();
    const visible = browseItems.filter((item) => !query || item.name.toLocaleLowerCase().includes(query));
    libraryBrowseItems.replaceChildren();
    visible.forEach((item) => {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'browse-item';
      button.dataset.browseUri = item.uri;
      button.textContent = item.name;
      libraryBrowseItems.append(button);
    });
    libraryBrowseStatus.textContent = `${visible.length} ${libraryBrowseKind.value}`;
  };

  const loadBrowseItems = async () => {
    if (!libraryBrowseKind) return;
    libraryBrowseStatus.textContent = `Loading ${libraryBrowseKind.value}…`;
    libraryBrowseTracks.replaceChildren();
    try {
      const response = await fetch(`/api/browse?kind=${encodeURIComponent(libraryBrowseKind.value)}`, {cache: 'no-store'});
      if (!response.ok) throw new Error((await response.text()).trim() || `HTTP ${response.status}`);
      const items = await response.json();
      browseItems = Array.isArray(items) ? items.filter((item) => item && item.uri && item.name) : [];
      renderBrowseItems();
    } catch (error) {
      browseItems = [];
      renderBrowseItems();
      libraryBrowseStatus.textContent = error.message || 'Library browse failed';
    }
  };

  const renderBrowseTracks = () => {
    libraryBrowseTracks.replaceChildren();
    browseTracks.forEach((track, index) => {
      const row = document.createElement('article');
      row.className = 'search-result';
      const copy = document.createElement('div');
      copy.className = 'search-result-copy';
      const heading = document.createElement('strong');
      heading.textContent = track.title || 'Unknown track';
      const details = document.createElement('span');
      details.textContent = [track.artist, track.album, formatTime((Number(track.duration_ms) || 0) / 1000)].filter(Boolean).join(' · ');
      copy.append(heading, details);
      const actions = document.createElement('div');
      actions.className = 'search-actions';
      const placements = guestMode ? [['end', 'Add']] : [['now', 'Play now'], ['next', 'Play next'], ['end', 'Add']];
      placements.forEach(([placement, label]) => {
        const button = document.createElement('button');
        button.type = 'button';
        button.textContent = label;
        button.dataset.browseTrackIndex = String(index);
        button.dataset.placement = placement;
        actions.append(button);
      });
      row.append(copy, actions);
      libraryBrowseTracks.append(row);
    });
  };

  const loadBrowseTracks = async (uri, name) => {
    libraryBrowseStatus.textContent = `Loading ${name}…`;
    try {
      const response = await fetch(`/api/lookup?uri=${encodeURIComponent(uri)}`, {cache: 'no-store'});
      if (!response.ok) throw new Error((await response.text()).trim() || `HTTP ${response.status}`);
      const tracks = await response.json();
      browseTracks = Array.isArray(tracks) ? tracks.filter((track) => track && track.uri) : [];
      renderBrowseTracks();
      libraryBrowseStatus.textContent = `${name} · ${browseTracks.length} track${browseTracks.length === 1 ? '' : 's'}`;
    } catch (error) {
      browseTracks = [];
      renderBrowseTracks();
      libraryBrowseStatus.textContent = error.message || 'Could not open selection';
    }
  };

  const renderQueue = (queue) => {
    if (!queueList) return;
    queueList.replaceChildren();
    const tracks = Array.isArray(queue) ? queue : [];
    const currentIndex = tracks.findIndex((track) => track.current);
    tracks.forEach((track, index) => {
      const row = document.createElement('article');
      row.className = `queue-item${track.current ? ' current' : ''}`;
      const copy = document.createElement('div');
      copy.className = 'search-result-copy queue-item-copy';
      const heading = document.createElement('strong');
      heading.textContent = `${track.current ? '▶ ' : ''}${track.title || 'Unknown track'}`;
      const detail = document.createElement('span');
      const social = [];
      if (track.requested_by) social.push(`Added by ${track.requested_by}`);
      if (Number(track.votes) > 0) social.push(`${track.votes} vote${Number(track.votes) === 1 ? '' : 's'}`);
      detail.textContent = [track.artist, track.album, ...social].filter(Boolean).join(' · ');
      copy.append(heading, detail);
      row.append(copy);
      const controls = document.createElement('div');
      controls.className = 'queue-controls';
      if (!guestMode) {
        const actions = document.createElement('div');
        actions.className = 'queue-actions';
        [['play', 'Play'], ['up', '↑'], ['down', '↓'], ['remove', 'Remove']].forEach(([action, label]) => {
          const button = document.createElement('button');
          button.type = 'button';
          button.textContent = label;
          button.dataset.queueAction = action;
          button.dataset.tlid = String(track.tlid);
          if ((action === 'up' && index === 0) || (action === 'down' && index + 1 === tracks.length)) button.disabled = true;
          actions.append(button);
        });
        controls.append(actions);
      }
      if (!track.current && (currentIndex < 0 || index > currentIndex)) {
        const vote = document.createElement('button');
        vote.type = 'button';
        vote.className = 'button-subtle queue-vote';
        vote.textContent = `▲ Vote${Number(track.votes) > 0 ? ` ${track.votes}` : ''}`;
        vote.dataset.queueVote = String(track.tlid);
        controls.append(vote);
      }
      if (controls.childElementCount > 0) row.append(controls);
      queueList.append(row);
    });
    queueStatus.textContent = tracks.length === 0 ? 'The queue is empty.' : `${tracks.length} track${tracks.length === 1 ? '' : 's'} · saved automatically`;
    if (queueClear) queueClear.disabled = tracks.length === 0;
  };

  const refreshQueue = async () => {
    if (!queueList) return;
    try {
      const response = await fetch('/api/queue', {cache: 'no-store'});
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      renderQueue(await response.json());
    } catch (_error) {
      queueStatus.textContent = 'Queue unavailable';
    }
  };

  const editQueue = async (action, tlid = '') => {
    try {
      const response = await fetch('/queue/edit', {
        method: 'POST',
        headers: {'Accept': 'application/json', 'Content-Type': 'application/x-www-form-urlencoded'},
        body: new URLSearchParams({csrf: csrf.value, action, tlid})
      });
      if (!response.ok) throw new Error((await response.text()).trim() || `HTTP ${response.status}`);
      window.setTimeout(refreshQueue, 200);
    } catch (error) {
      showFeedback(error.message || 'Queue edit failed', true);
    }
  };

  const voteQueue = async (button, tlid) => {
    button.disabled = true;
    try {
      const response = await fetch('/queue/vote', {
        method: 'POST',
        headers: {'Accept': 'application/json', 'Content-Type': 'application/x-www-form-urlencoded'},
        body: new URLSearchParams({csrf: csrf.value, tlid})
      });
      if (!response.ok) throw new Error((await response.text()).trim() || `HTTP ${response.status}`);
      showFeedback('Vote counted');
      window.setTimeout(refreshQueue, 150);
    } catch (error) {
      showFeedback(error.message || 'Could not vote', true);
      button.disabled = false;
    }
  };

  const searchLibrary = async () => {
    const query = librarySearchInput.value.trim();
    window.clearTimeout(searchTimer);
    if (query.length < 3) {
      if (searchController) searchController.abort();
      renderSearchResults([]);
      librarySearchStatus.textContent = 'Enter at least 3 characters.';
      return;
    }
    if (searchController) searchController.abort();
    const controller = new AbortController();
    searchController = controller;
    librarySearchButton.disabled = true;
    librarySearchStatus.textContent = 'Searching Jellyfin…';
    try {
      const response = await fetch(`/api/search?q=${encodeURIComponent(query)}`, {cache: 'no-store', signal: controller.signal});
      if (!response.ok) throw new Error((await response.text()).trim() || `HTTP ${response.status}`);
      const tracks = await response.json();
      if (!Array.isArray(tracks)) throw new Error('Invalid search response');
      renderSearchResults(tracks.filter((track) => track && track.uri && track.title));
      librarySearchStatus.textContent = searchTracks.length === 0 ? 'No matching Jellyfin songs.' : `${searchTracks.length} song${searchTracks.length === 1 ? '' : 's'} found`;
    } catch (error) {
      if (error.name === 'AbortError') return;
      renderSearchResults([]);
      librarySearchStatus.textContent = error.message || 'Jellyfin search failed';
    } finally {
      if (searchController === controller) {
        searchController = null;
        librarySearchButton.disabled = latestPlayback.mopidy_online === false;
      }
    }
  };

  const queueSearchTrack = async (button, track, placement) => {
    const name = requesterName ? requesterName.value.trim() : '';
    if (!name) {
      showFeedback('Enter your name before adding a song', true);
      const changeName = get('change-client-name');
      if (changeName) changeName.click();
      return;
    }
    button.disabled = true;
    try {
      const response = await fetch('/queue', {
        method: 'POST',
        headers: {'Accept': 'application/json', 'Content-Type': 'application/x-www-form-urlencoded'},
        body: new URLSearchParams({csrf: librarySearchCsrf.value, track_uri: track.uri, placement, requester_name: name})
      });
      if (!response.ok) throw new Error((await response.text()).trim() || `HTTP ${response.status}`);
      const action = placement === 'now' ? 'Playing' : placement === 'next' ? 'Queued next' : 'Added to queue';
      showFeedback(`${action}: ${track.title}`);
      window.setTimeout(refreshQueue, 200);
      if (placement === 'now') window.setTimeout(refresh, 250);
    } catch (error) {
      showFeedback(error.message || 'Could not update queue', true);
    } finally {
      button.disabled = latestPlayback.mopidy_online === false;
    }
  };

  const renderPosition = (position, duration) => {
    const safeDuration = Math.max(0, Number(duration) || 0);
    const safePosition = Math.max(0, Number(position) || 0);
    const percent = safeDuration > 0 ? Math.min(100, safePosition / safeDuration * 100) : 0;
    progress.style.width = `${percent}%`;
    progressBar.setAttribute('aria-valuenow', String(Math.round(percent)));
    progressBar.setAttribute('aria-valuetext', `${formatTime(safePosition)} of ${formatTime(safeDuration)}`);
    elapsed.textContent = formatTime(safePosition);
    remaining.textContent = safeDuration > 0 ? `-${formatTime(Math.max(0, safeDuration - safePosition))}` : '--:--';
  };

  const updateArtwork = (playback) => {
    const artworkId = playback.artwork_id || null;
    artwork.alt = artworkId ? `Album artwork for ${playback.title || 'current track'}` : '';
    if (artworkId === currentArtworkId) return;
    currentArtworkId = artworkId;
    if (!artworkId) {
      artworkWrap.hidden = true;
      artwork.removeAttribute('src');
      return;
    }
    artworkWrap.hidden = false;
    artwork.src = `/api/artwork?v=${encodeURIComponent(artworkId)}`;
  };

  const render = (playback) => {
    latestPlayback = playback;
    const online = Boolean(playback.online);
    const source = playback.source || 'Mopidy';
    status.textContent = !online ? `${source} unavailable` : `${playback.is_playing ? 'Playing' : 'Paused / stopped'} · ${source}`;
    status.classList.toggle('offline', !online);
    title.textContent = playback.title || 'No track playing';
    meta.textContent = [playback.artist, playback.album].filter(Boolean).join(' · ') || '—';
    toggle.textContent = playback.is_playing ? 'Pause' : 'Play';
    document.title = playback.title && playback.title !== 'No track playing' ? `${playback.title} — Orpheus` : 'Orpheus';
    setOnline(online, Boolean(playback.mopidy_online));
    if (quickPlaylistPlay && playlistSelect) quickPlaylistPlay.disabled = !playback.mopidy_online || playlistSelect.options.length === 0;
    renderPosition(playback.position_seconds, playback.duration_seconds);
    updateArtwork(playback);

    const nextDescription = [playback.next_artist, playback.next_title].filter(Boolean).join(' — ');
    next.textContent = nextDescription ? `Next: ${nextDescription}` : 'Next: queue end';
    if (screenToggle) screenToggle.textContent = playback.screensaver_active ? 'Wake display' : 'Preview screensaver';
    if (carouselStatus) {
      const count = Math.max(0, Number(playback.carousel_cover_count) || 0);
      const speed = Math.max(0, Number(playback.carousel_speed) || 0);
      carouselStatus.textContent = `${count} album cover${count === 1 ? '' : 's'} ready · ${Math.round(speed)} px/s`;
    }

    if (playback.volume !== null && playback.volume !== undefined) {
      const maximum = Math.max(10, Math.min(100, Number(playback.max_volume) || 100));
      const level = Math.max(0, Math.min(maximum, Number(playback.volume) || 0));
      volume.max = String(maximum);
      if (pendingVolume !== null && level === pendingVolume) pendingVolume = null;
      if (pendingVolume === null && !volumeDragging && Date.now() >= volumeEditingUntil) {
        volume.value = String(level);
        volumeLabel.textContent = `Volume (${level}%)`;
      }
    } else {
      volumeLabel.textContent = 'Volume unavailable';
    }
  };

  const refresh = async () => {
    if (document.hidden) return;
    try {
      const response = await fetch('/api/status', {cache: 'no-store'});
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      render(await response.json());
    } catch (_error) {
      status.textContent = 'Status unavailable';
      status.classList.add('offline');
      setOnline(false, false);
    }
  };

  const connectLiveUpdates = () => {
    if (!('EventSource' in window)) return;
    const events = new EventSource('/api/events');
    events.addEventListener('state', (event) => {
      try {
        const state = JSON.parse(event.data);
        if (state.playback) render(state.playback);
        if (state.queue) renderQueue(state.queue);
      } catch (_error) {}
    });
  };

  const sendAction = async (action, fields = {}) => {
    if (action !== 'screensaver' && latestPlayback.online === false) {
      showFeedback(`${latestPlayback.source || 'Playback source'} is unavailable`, true);
      return false;
    }
    const body = new URLSearchParams({csrf: csrf.value, action, ...fields});
    try {
      const response = await fetch(playbackEndpoint, {
        method: 'POST',
        headers: {'Accept': 'application/json', 'Content-Type': 'application/x-www-form-urlencoded'},
        body
      });
      if (!response.ok) throw new Error((await response.text()).trim() || `HTTP ${response.status}`);
      const label = action === 'seek' ? `Seeking to ${formatTime(fields.position_seconds)}` : action === 'volume' ? `Volume ${fields.volume}%` : action === 'screensaver' ? 'Display command sent' : 'Command sent';
      showFeedback(label);
      if (action !== 'volume') window.setTimeout(refresh, 150);
      return true;
    } catch (error) {
      showFeedback(error.message || 'Command failed', true);
      return false;
    }
  };

  const seekTo = (position) => {
    const duration = Math.max(0, Number(latestPlayback.duration_seconds) || 0);
    if (duration <= 0) return;
    const target = Math.round(Math.max(0, Math.min(duration, position)));
    latestPlayback.position_seconds = target;
    renderPosition(target, duration);
    sendAction('seek', {position_seconds: String(target)});
  };

  const commitWebVolume = async (target) => {
    const sent = await sendAction('volume', {volume: String(target)});
    if (!sent && pendingVolume === target) {
      pendingVolume = null;
      refresh();
      return;
    }
    window.setTimeout(() => {
      if (pendingVolume === target) {
        pendingVolume = null;
        refresh();
      }
    }, 4000);
  };

  const setWebVolume = (level, immediate = true) => {
    const maximum = Math.max(10, Math.min(100, Number(latestPlayback.max_volume) || 100));
    const target = Math.round(Math.max(0, Math.min(maximum, level)));
    pendingVolume = target;
    volumeEditingUntil = Date.now() + 2000;
    latestPlayback.volume = target;
    volume.value = String(target);
    volumeLabel.textContent = `Volume (${target}%)`;
    window.clearTimeout(volumeTimer);
    if (immediate) {
      commitWebVolume(target);
    } else {
      volumeTimer = window.setTimeout(() => commitWebVolume(target), 120);
    }
  };

  playbackForm.addEventListener('submit', (event) => {
    const action = event.submitter && event.submitter.value;
    if (!action) return;
    event.preventDefault();
    if (guestMode && action !== 'toggle' && action !== 'volume') return;
    const fields = action === 'volume' ? {volume: volume.value} : {};
    sendAction(action, fields);
  });
  volume.addEventListener('pointerdown', () => { volumeDragging = true; });
  volume.addEventListener('pointerup', () => {
    volumeDragging = false;
    volumeEditingUntil = Date.now() + 2000;
  });
  volume.addEventListener('pointercancel', () => { volumeDragging = false; });
  volume.addEventListener('input', () => setWebVolume(Number(volume.value), false));
  volume.addEventListener('change', () => setWebVolume(Number(volume.value), true));
  progressBar.addEventListener('click', (event) => {
    if (guestMode) return;
    const duration = Math.max(0, Number(latestPlayback.duration_seconds) || 0);
    if (duration <= 0) return;
    const bounds = progressBar.getBoundingClientRect();
    seekTo((event.clientX - bounds.left) / bounds.width * duration);
  });
  artwork.addEventListener('error', () => { artworkWrap.hidden = true; });

  if (playlistFilter) playlistFilter.addEventListener('input', renderPlaylistOptions);
  if (quickPlaylistForm && playlistSelect && quickPlaylistCsrf && quickPlaylistPlay) {
    quickPlaylistForm.addEventListener('submit', async (event) => {
      event.preventDefault();
      if (!latestPlayback.mopidy_online || !playlistSelect.value) return;
      const selected = playlistSelect.selectedOptions[0];
      const playlistName = selected && selected.dataset.name || selected && selected.textContent || 'playlist';
      quickPlaylistPlay.disabled = true;
      quickPlaylistPlay.textContent = 'Starting…';
      try {
        const response = await fetch(quickPlaylistForm.action, {
          method: 'POST',
          headers: {'Accept': 'application/json', 'Content-Type': 'application/x-www-form-urlencoded'},
          body: new URLSearchParams({csrf: quickPlaylistCsrf.value, playlist_uri: playlistSelect.value})
        });
        if (!response.ok) throw new Error((await response.text()).trim() || `HTTP ${response.status}`);
        showFeedback(`Starting ${playlistName}`);
        window.setTimeout(refresh, 300);
      } catch (error) {
        showFeedback(error.message || 'Could not start playlist', true);
      } finally {
        quickPlaylistPlay.textContent = 'Play selected';
        quickPlaylistPlay.disabled = !latestPlayback.mopidy_online || playlistSelect.options.length === 0;
      }
    });
  }

  librarySearchForm.addEventListener('submit', (event) => {
    event.preventDefault();
    searchLibrary();
  });
  librarySearchInput.addEventListener('input', () => {
    window.clearTimeout(searchTimer);
    const query = librarySearchInput.value.trim();
    if (query.length < 3) {
      if (searchController) searchController.abort();
      renderSearchResults([]);
      librarySearchStatus.textContent = query.length === 0 ? 'Search your Jellyfin music library.' : 'Enter at least 3 characters.';
      return;
    }
    searchTimer = window.setTimeout(searchLibrary, 350);
  });
  librarySearchResults.addEventListener('click', (event) => {
    const button = event.target.closest('button[data-search-index]');
    if (!button) return;
    const track = searchTracks[Number(button.dataset.searchIndex)];
    if (track) queueSearchTrack(button, track, button.dataset.placement);
  });
  if (libraryBrowseKind) {
    libraryBrowseKind.addEventListener('change', loadBrowseItems);
    libraryBrowseFilter.addEventListener('input', renderBrowseItems);
    libraryBrowseItems.addEventListener('click', (event) => {
      const button = event.target.closest('button[data-browse-uri]');
      if (button) loadBrowseTracks(button.dataset.browseUri, button.textContent || 'Selection');
    });
    libraryBrowseTracks.addEventListener('click', (event) => {
      const button = event.target.closest('button[data-browse-track-index]');
      if (!button) return;
      const track = browseTracks[Number(button.dataset.browseTrackIndex)];
      if (track) queueSearchTrack(button, track, button.dataset.placement);
    });
    loadBrowseItems();
  }

  if (queueList) {
    queueList.addEventListener('click', (event) => {
      const vote = event.target.closest('button[data-queue-vote]');
      if (vote) { voteQueue(vote, vote.dataset.queueVote); return; }
      const button = event.target.closest('button[data-queue-action]');
      if (button) editQueue(button.dataset.queueAction, button.dataset.tlid);
    });
    if (queueClear) queueClear.addEventListener('click', () => {
      if (window.confirm('Clear the entire Mopidy queue?')) editQueue('clear');
    });
  }

  document.addEventListener('keydown', (event) => {
    const target = event.target;
    if (target instanceof HTMLElement && (target.isContentEditable || target.matches('input, select, textarea, button'))) return;
    if (guestMode && ![' ', 'ArrowUp', 'ArrowDown'].includes(event.key)) return;
    let handled = true;
    const position = Number(latestPlayback.position_seconds) || 0;
    const level = Number(latestPlayback.volume) || 0;
    switch (event.key) {
      case ' ': sendAction('toggle'); break;
      case 'ArrowLeft': seekTo(position - 10); break;
      case 'ArrowRight': seekTo(position + 10); break;
      case 'ArrowUp': setWebVolume(level + 5); break;
      case 'ArrowDown': setWebVolume(level - 5); break;
      case 'n': case 'N': sendAction('next'); break;
      case 'p': case 'P': sendAction('previous'); break;
      case 's': case 'S': sendAction('screensaver'); break;
      default: handled = false;
    }
    if (handled) event.preventDefault();
  });
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) refresh();
  });
  refresh();
  refreshPlaylists();
  refreshQueue();
  connectLiveUpdates();
  window.setInterval(refresh, 10000);
  window.setInterval(refreshPlaylists, 60000);
  window.setInterval(refreshQueue, 10000);
})();
"#;

const SETTINGS_JS: &str = r#"(() => {
  const get = (id) => document.getElementById(id);
  const settingsForm = get('settings-form');
  const feedback = get('settings-feedback');
  const csrf = settingsForm && settingsForm.querySelector('input[name="csrf"]');
  const visualizerDelay = get('visualizer-delay');
  const visualizerDelayLabel = get('visualizer-delay-label');
  const spotifyVisualizerDelay = get('spotify-visualizer-delay');
  const spotifyVisualizerDelayLabel = get('spotify-visualizer-delay-label');
  const carouselSpeed = get('carousel-speed');
  const carouselSpeedLabel = get('carousel-speed-label');
  let visualizerTimer;
  let spotifyVisualizerTimer;
  let carouselTimer;
  let feedbackTimer;

  const showFeedback = (message, isError = false) => {
    if (!feedback) return;
    window.clearTimeout(feedbackTimer);
    feedback.textContent = message;
    feedback.classList.toggle('error', isError);
    feedbackTimer = window.setTimeout(() => { feedback.textContent = ''; }, 2500);
  };

  const saveRange = async (path, field, value, success) => {
    try {
      const response = await fetch(path, {
        method: 'POST',
        headers: {'Accept': 'application/json', 'Content-Type': 'application/x-www-form-urlencoded'},
        body: new URLSearchParams({csrf: csrf.value, [field]: value})
      });
      if (!response.ok) throw new Error((await response.text()).trim() || `HTTP ${response.status}`);
      showFeedback(success);
    } catch (error) {
      showFeedback(error.message || 'Could not apply setting', true);
    }
  };

  if (csrf && visualizerDelay && visualizerDelayLabel) {
    visualizerDelay.addEventListener('input', () => {
      visualizerDelayLabel.textContent = `Visualizer delay (${visualizerDelay.value} ms)`;
      window.clearTimeout(visualizerTimer);
      visualizerTimer = window.setTimeout(() => saveRange(
        '/visualizer-delay',
        'visualizer_delay_ms',
        visualizerDelay.value,
        `Visualizer delay ${visualizerDelay.value} ms`
      ), 200);
    });
  }

  if (csrf && spotifyVisualizerDelay && spotifyVisualizerDelayLabel) {
    spotifyVisualizerDelay.addEventListener('input', () => {
      spotifyVisualizerDelayLabel.textContent = `Spotify extra delay (+${spotifyVisualizerDelay.value} ms)`;
      window.clearTimeout(spotifyVisualizerTimer);
      spotifyVisualizerTimer = window.setTimeout(() => saveRange(
        '/spotify-visualizer-delay',
        'spotify_visualizer_extra_delay_ms',
        spotifyVisualizerDelay.value,
        `Spotify visualizer offset +${spotifyVisualizerDelay.value} ms`
      ), 200);
    });
  }

  if (csrf && carouselSpeed && carouselSpeedLabel) {
    carouselSpeed.addEventListener('input', () => {
      const value = Number(carouselSpeed.value) || 0;
      carouselSpeedLabel.textContent = value === 0 ? 'Carousel speed (paused)' : `Carousel speed (${value} px/s)`;
      window.clearTimeout(carouselTimer);
      carouselTimer = window.setTimeout(() => saveRange(
        '/carousel-speed',
        'carousel_speed',
        carouselSpeed.value,
        value === 0 ? 'Carousel paused' : `Carousel speed ${value} px/s`
      ), 200);
    });
  }
})();
"#;

const PWA_JS: &str = r#"(() => {
  const button = document.getElementById('install-app');
  const status = document.getElementById('install-status');
  const standalone = window.matchMedia('(display-mode: standalone)').matches || window.navigator.standalone === true;
  const isIos = /iphone|ipad|ipod/i.test(navigator.userAgent);
  let installPrompt = null;

  const setStatus = (message) => {
    if (status) status.textContent = message;
  };

  if ('serviceWorker' in navigator && window.isSecureContext) {
    navigator.serviceWorker.register('/service-worker.js').catch(() => {
      setStatus('The app shell could not be registered. You can still use the browser version.');
    });
  }

  if (standalone) {
    if (button) {
      button.textContent = 'Application installed';
      button.disabled = true;
    }
    setStatus('Orpheus is running as an installed application.');
  } else if (!window.isSecureContext) {
    setStatus('On this local HTTP address, use your browser menu and choose Add to Home screen. A direct install prompt requires HTTPS.');
  }

  window.addEventListener('beforeinstallprompt', (event) => {
    event.preventDefault();
    installPrompt = event;
    if (button) {
      button.disabled = false;
      button.textContent = 'Install Orpheus';
    }
    setStatus('This browser is ready to install Orpheus.');
  });

  if (button) button.addEventListener('click', async () => {
    if (installPrompt) {
      installPrompt.prompt();
      await installPrompt.userChoice;
      installPrompt = null;
      return;
    }
    setStatus(isIos
      ? 'In Safari, tap Share, then Add to Home Screen.'
      : 'Open the browser menu and choose Install app or Add to Home screen.');
  });

  window.addEventListener('appinstalled', () => {
    installPrompt = null;
    if (button) {
      button.textContent = 'Application installed';
      button.disabled = true;
    }
    setStatus('Orpheus was installed successfully.');
  });
})();
"#;

const SERVICE_WORKER_JS: &str = r#"const CACHE_NAME = 'orpheus-shell-v9';
const SHELL_ASSETS = [
  '/app.js?v=9',
  '/identity.js?v=1',
  '/settings.js',
  '/pwa.js',
  '/manifest.webmanifest',
  '/icon-192.png',
  '/icon-512.png'
];

self.addEventListener('install', (event) => {
  event.waitUntil(caches.open(CACHE_NAME).then((cache) => cache.addAll(SHELL_ASSETS)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (event) => {
  event.waitUntil(caches.keys()
    .then((keys) => Promise.all(keys.filter((key) => key !== CACHE_NAME).map((key) => caches.delete(key))))
    .then(() => self.clients.claim()));
});

self.addEventListener('fetch', (event) => {
  if (event.request.method !== 'GET') return;
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin || event.request.mode === 'navigate' || url.pathname.startsWith('/api/')) return;
  event.respondWith(fetch(event.request)
    .then((response) => {
      const copy = response.clone();
      caches.open(CACHE_NAME).then((cache) => cache.put(event.request, copy));
      return response;
    })
    .catch(() => caches.match(event.request)));
});
"#;

const WEB_MANIFEST: &str = r##"{
  "id": "/",
  "name": "Orpheus",
  "short_name": "Orpheus",
  "description": "A lightweight local music controller for Mopidy, Spotify Connect, playlists, and the Orpheus display.",
  "start_url": "/",
  "scope": "/",
  "display": "standalone",
  "background_color": "#0d0e10",
  "theme_color": "#0d0e10",
  "icons": [
    {"src": "/icon-192.png", "sizes": "192x192", "type": "image/png", "purpose": "any maskable"},
    {"src": "/icon-512.png", "sizes": "512x512", "type": "image/png", "purpose": "any maskable"}
  ],
  "shortcuts": [
    {"name": "Playback", "short_name": "Player", "url": "/", "icons": [{"src": "/icon-192.png", "sizes": "192x192", "type": "image/png"}]},
    {"name": "History", "short_name": "History", "url": "/history", "icons": [{"src": "/icon-192.png", "sizes": "192x192", "type": "image/png"}]},
    {"name": "Diagnostics", "short_name": "Status", "url": "/diagnostics", "icons": [{"src": "/icon-192.png", "sizes": "192x192", "type": "image/png"}]},
    {"name": "Configuration", "short_name": "Settings", "url": "/settings", "icons": [{"src": "/icon-192.png", "sizes": "192x192", "type": "image/png"}]}
  ]
}
"##;

#[derive(Debug)]
pub enum WebConfigUpdate {
    Settings(Settings),
    Playlists(Config),
    PlayPlaylist(PlaylistEntry),
    Playback(WebPlaybackAction),
    QueueTrack {
        uri: String,
        placement: QueuePlacement,
        requested_by: Option<String>,
    },
    QueueVote(u64),
    QueueEdit(WebQueueAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebQueueAction {
    Remove(u64),
    MoveUp(u64),
    MoveDown(u64),
    Play(u64),
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebPlaybackAction {
    Previous,
    TogglePlayPause,
    Next,
    Seek(u64),
    SetVolume(u8),
    ToggleScreensaver,
}

#[derive(Debug, Clone, Serialize)]
pub struct WebPlaybackStatus {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub is_playing: bool,
    pub online: bool,
    pub source: String,
    pub mopidy_online: bool,
    pub spotifyd_available: bool,
    pub volume: Option<u8>,
    pub max_volume: u8,
    pub position_seconds: f64,
    pub duration_seconds: f64,
    pub next_title: Option<String>,
    pub next_artist: Option<String>,
    pub artwork_id: Option<String>,
    #[serde(skip)]
    pub artwork_path: Option<String>,
    pub screensaver_active: bool,
    pub carousel_cover_count: usize,
    pub carousel_speed: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebPlaylist {
    pub name: String,
    pub uri: String,
    pub favorite: bool,
}

impl Default for WebPlaybackStatus {
    fn default() -> Self {
        Self {
            title: "No track playing".to_string(),
            artist: "—".to_string(),
            album: "—".to_string(),
            is_playing: false,
            online: false,
            source: "Mopidy".to_string(),
            mopidy_online: false,
            spotifyd_available: false,
            volume: None,
            max_volume: Settings::default().max_volume,
            position_seconds: 0.0,
            duration_seconds: 0.0,
            next_title: None,
            next_artist: None,
            artwork_id: None,
            artwork_path: None,
            screensaver_active: false,
            carousel_cover_count: 0,
            carousel_speed: Settings::default().carousel_speed,
        }
    }
}

pub struct WebConfigBridge {
    pub updates: Receiver<WebConfigUpdate>,
    pub playback_status: Arc<Mutex<WebPlaybackStatus>>,
    pub playlist_catalog: Arc<Mutex<Vec<WebPlaylist>>>,
    pub queue: Arc<Mutex<Vec<MopidyQueueTrack>>>,
    pub history: Arc<Mutex<Vec<HistoryEntry>>>,
}

struct HttpRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone)]
struct WebServerState {
    settings_path: PathBuf,
    playlists_path: PathBuf,
    csrf_token: Arc<String>,
    updates: Sender<WebConfigUpdate>,
    playback_status: Arc<Mutex<WebPlaybackStatus>>,
    playlist_catalog: Arc<Mutex<Vec<WebPlaylist>>>,
    library_search: Arc<LibrarySearch>,
    library_browse: Arc<LibraryBrowse>,
    library_lookup: Arc<LibraryLookup>,
    recent_search_uris: Arc<Mutex<VecDeque<String>>>,
    settings_sessions: Arc<Mutex<HashMap<String, Instant>>>,
    failed_logins: Arc<Mutex<VecDeque<Instant>>>,
    guest_queue_attempts: Arc<Mutex<HashMap<String, Instant>>>,
    queue_reservations: Arc<Mutex<HashMap<String, Instant>>>,
    queue_voters: Arc<Mutex<HashMap<u64, BTreeSet<String>>>>,
    queue: Arc<Mutex<Vec<MopidyQueueTrack>>>,
    history: Arc<Mutex<Vec<HistoryEntry>>>,
    active_clients: Arc<AtomicUsize>,
    started_at: Instant,
}

pub fn spawn(
    bind_address: &str,
    settings_path: PathBuf,
    playlists_path: PathBuf,
    library_client: MopidyLibraryClient,
) -> WebConfigBridge {
    let (updates_tx, updates_rx) = mpsc::channel();
    let playback_status = Arc::new(Mutex::new(WebPlaybackStatus::default()));
    let server_playback_status = Arc::clone(&playback_status);
    let playlist_catalog = Arc::new(Mutex::new(Vec::new()));
    let server_playlist_catalog = Arc::clone(&playlist_catalog);
    let search_client = library_client.clone();
    let browse_client = library_client.clone();
    let library_search: Arc<LibrarySearch> =
        Arc::new(move |query, limit| search_client.search_jellyfin_tracks(query, limit));
    let library_browse: Arc<LibraryBrowse> =
        Arc::new(move |kind, limit| browse_client.browse_jellyfin(kind, limit));
    let library_lookup: Arc<LibraryLookup> =
        Arc::new(move |uri, limit| library_client.lookup_jellyfin_tracks(uri, limit));
    let queue = Arc::new(Mutex::new(Vec::new()));
    let server_queue = Arc::clone(&queue);
    let history = Arc::new(Mutex::new(Vec::new()));
    let server_history = Arc::clone(&history);
    let bind_address = bind_address.to_string();
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(&bind_address) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("[WEB] Failed to listen on {bind_address}: {error}");
                return;
            }
        };
        let server = WebServerState {
            settings_path,
            playlists_path,
            csrf_token: Arc::new(generate_csrf_token()),
            updates: updates_tx,
            playback_status: server_playback_status,
            playlist_catalog: server_playlist_catalog,
            library_search,
            library_browse,
            library_lookup,
            recent_search_uris: Arc::new(Mutex::new(VecDeque::new())),
            settings_sessions: Arc::new(Mutex::new(HashMap::new())),
            failed_logins: Arc::new(Mutex::new(VecDeque::new())),
            guest_queue_attempts: Arc::new(Mutex::new(HashMap::new())),
            queue_reservations: Arc::new(Mutex::new(HashMap::new())),
            queue_voters: Arc::new(Mutex::new(HashMap::new())),
            queue: server_queue,
            history: server_history,
            active_clients: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
        };
        println!("[WEB] Configuration available at http://{bind_address}");

        for connection in listener.incoming() {
            match connection {
                Ok(mut stream) => {
                    let server = server.clone();
                    std::thread::spawn(move || {
                        if let Err(error) = serve_connection(&mut stream, &server) {
                            eprintln!("[WEB] Request failed: {error}");
                        }
                    });
                }
                Err(error) => eprintln!("[WEB] Connection failed: {error}"),
            }
        }
    });
    WebConfigBridge {
        updates: updates_rx,
        playback_status,
        playlist_catalog,
        queue,
        history,
    }
}

fn serve_connection(stream: &mut TcpStream, server: &WebServerState) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|error| error.to_string())?;
    let request = read_request(stream)?;
    let client_id = stream
        .peer_addr()
        .map(|address| address.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    dispatch_request_for_client(stream, request, server, &client_id)
}

#[cfg(test)]
fn dispatch_request<W: Write>(
    stream: &mut W,
    request: HttpRequest,
    server: &WebServerState,
) -> Result<(), String> {
    dispatch_request_for_client(stream, request, server, "test-client")
}

fn dispatch_request_for_client<W: Write>(
    stream: &mut W,
    request: HttpRequest,
    server: &WebServerState,
    client_id: &str,
) -> Result<(), String> {
    let settings_path = &server.settings_path;
    let playlists_path = &server.playlists_path;
    let csrf_token = server.csrf_token.as_str();
    let updates = &server.updates;
    let playback_status = &server.playback_status;
    let playlist_catalog = &server.playlist_catalog;
    let path = request.target.split('?').next().unwrap_or("/");
    let authenticated = settings_request_is_authenticated(&request, &server.settings_sessions);
    let access_settings = load_access_settings(settings_path);
    let guest_restricted = access_settings.guest_mode_enabled && !authenticated;

    if settings_route_requires_auth(request.method.as_str(), path) && !authenticated {
        return if accepts_json(&request) {
            respond_text(stream, 401, "Unauthorized", "Settings login required\n")
        } else {
            respond_redirect(stream, "/login")
        };
    }

    if guest_restricted
        && request.method == "POST"
        && matches!(path, "/library/play" | "/playlists/play" | "/queue/edit")
    {
        return respond_text(
            stream,
            403,
            "Forbidden",
            "Guest mode only allows adding a searched song to the end of the queue\n",
        );
    }

    if guest_restricted
        && request.method == "POST"
        && path == "/playback"
        && !guest_playback_request_is_allowed(&request)
    {
        return respond_text(
            stream,
            403,
            "Forbidden",
            "Guests may only play or pause and change the volume\n",
        );
    }

    match (request.method.as_str(), path) {
        ("GET", "/") => {
            let playback = playback_status
                .lock()
                .map(|status| status.clone())
                .unwrap_or_default();
            let playlists = playlist_catalog
                .lock()
                .map(|catalog| catalog.clone())
                .unwrap_or_default();
            let notice = query_parameter(&request.target, "sent")
                .map(|_| "Playback command sent to Mopidy.");
            respond_html(
                stream,
                200,
                "OK",
                &render_player_page(
                    &playback,
                    &playlists,
                    csrf_token,
                    notice,
                    guest_restricted,
                    access_settings.guest_queue_cooldown_seconds,
                ),
            )
        }
        ("GET", "/login") => {
            if settings_request_is_authenticated(&request, &server.settings_sessions) {
                return respond_redirect(stream, "/settings");
            }
            let notice = query_parameter(&request.target, "changed")
                .map(|_| "Password changed. Sign in again with the new password.");
            respond_html(
                stream,
                200,
                "OK",
                &render_login_page(csrf_token, None, notice),
            )
        }
        ("POST", "/login") => match login_request(
            &request,
            settings_path,
            csrf_token,
            &server.settings_sessions,
            &server.failed_logins,
        ) {
            Ok(session) => respond_session_redirect(stream, "/settings", Some(&session)),
            Err(LoginError::RateLimited) => respond_html(
                stream,
                429,
                "Too Many Requests",
                &render_login_page(
                    csrf_token,
                    Some("Too many attempts. Please wait a minute."),
                    None,
                ),
            ),
            Err(LoginError::Rejected) => respond_html(
                stream,
                401,
                "Unauthorized",
                &render_login_page(csrf_token, Some("That password was not accepted."), None),
            ),
            Err(LoginError::Unavailable(error)) => respond_text(
                stream,
                503,
                "Service Unavailable",
                &format!("Settings login is unavailable: {error}\n"),
            ),
        },
        ("POST", "/logout") => {
            logout_request(&request, csrf_token, &server.settings_sessions)?;
            respond_session_redirect(stream, "/login", None)
        }
        ("GET", "/settings") => {
            let settings = Settings::load(&settings_path.to_string_lossy());
            let config = Config::load(&playlists_path.to_string_lossy());
            let notice = query_parameter(&request.target, "saved").map(|section| match section {
                "settings" => "Settings saved and applied.",
                "playlists" => "Playlists saved and reloaded.",
                _ => "Configuration saved.",
            });
            respond_html(
                stream,
                200,
                "OK",
                &render_settings_page(&settings, &config, csrf_token, notice),
            )
        }
        ("GET", "/history") => {
            let history = server
                .history
                .lock()
                .map(|history| history.clone())
                .unwrap_or_default();
            respond_html(
                stream,
                200,
                "OK",
                &render_history_page(&history, csrf_token),
            )
        }
        ("GET", "/diagnostics") => {
            let diagnostics = collect_diagnostics(server);
            respond_html(stream, 200, "OK", &render_diagnostics_page(&diagnostics))
        }
        ("GET", "/health") => respond_text(stream, 200, "OK", "ok\n"),
        ("GET", "/api/status") => {
            let playback = playback_status
                .lock()
                .map(|status| status.clone())
                .unwrap_or_default();
            let json = serde_json::to_string(&playback)
                .map_err(|error| format!("could not serialize playback status: {error}"))?;
            respond_json(stream, 200, "OK", &json)
        }
        ("GET", "/api/status/summary") => {
            let json = serde_json::to_string(&collect_public_status(server))
                .map_err(|error| format!("could not serialize public status: {error}"))?;
            respond_public_json(stream, 200, "OK", &json)
        }
        ("OPTIONS", "/api/status/summary") => respond(
            stream,
            204,
            "No Content",
            "application/json; charset=utf-8",
            "",
            &[
                ("Access-Control-Allow-Origin", "*"),
                ("Access-Control-Allow-Methods", "GET, OPTIONS"),
                ("Access-Control-Allow-Headers", "Accept, Content-Type"),
                ("Access-Control-Max-Age", "86400"),
            ],
        ),
        ("GET", "/api/playlists") => {
            let playlists = playlist_catalog
                .lock()
                .map(|catalog| catalog.clone())
                .unwrap_or_default();
            let json = serde_json::to_string(&playlists)
                .map_err(|error| format!("could not serialize playlist catalog: {error}"))?;
            respond_json(stream, 200, "OK", &json)
        }
        ("GET", "/api/queue") => {
            let queue = server
                .queue
                .lock()
                .map(|queue| queue.clone())
                .unwrap_or_default();
            let json = serde_json::to_string(&queue)
                .map_err(|error| format!("could not serialize queue: {error}"))?;
            respond_json(stream, 200, "OK", &json)
        }
        ("GET", "/api/events") => respond_event_stream(stream, server),
        ("GET", "/api/history") => {
            let history = server
                .history
                .lock()
                .map(|history| history.clone())
                .unwrap_or_default();
            let json = serde_json::to_string(&history)
                .map_err(|error| format!("could not serialize playback history: {error}"))?;
            respond_json(stream, 200, "OK", &json)
        }
        ("GET", "/api/diagnostics") => {
            let json = serde_json::to_string(&collect_diagnostics(server))
                .map_err(|error| format!("could not serialize diagnostics: {error}"))?;
            respond_json(stream, 200, "OK", &json)
        }
        ("GET", "/api/search") => {
            let query = match decoded_query_parameter(&request.target, "q") {
                Some(query) if query.trim().chars().count() >= 3 => query,
                _ => {
                    return respond_text(
                        stream,
                        400,
                        "Bad Request",
                        "Search text must contain at least 3 characters\n",
                    );
                }
            };
            if query.chars().count() > 120 {
                return respond_text(stream, 400, "Bad Request", "Search text is too long\n");
            }
            match (server.library_search)(query.trim(), MAX_LIBRARY_SEARCH_RESULTS) {
                Ok(tracks) => {
                    remember_search_uris(&server.recent_search_uris, &tracks);
                    let json = serde_json::to_string(&tracks)
                        .map_err(|error| format!("could not serialize search results: {error}"))?;
                    respond_json(stream, 200, "OK", &json)
                }
                Err(error) => respond_text(
                    stream,
                    502,
                    "Bad Gateway",
                    &format!("Jellyfin search is unavailable: {error}\n"),
                ),
            }
        }
        ("GET", "/api/browse") => {
            let kind = decoded_query_parameter(&request.target, "kind")
                .ok_or_else(|| "missing browse kind".to_string())?;
            match (server.library_browse)(kind.trim(), 2_000) {
                Ok(items) => {
                    let json = serde_json::to_string(&items)
                        .map_err(|error| format!("could not serialize browse results: {error}"))?;
                    respond_json(stream, 200, "OK", &json)
                }
                Err(error) => respond_text(stream, 502, "Bad Gateway", &format!("{error}\n")),
            }
        }
        ("GET", "/api/lookup") => {
            let uri = decoded_query_parameter(&request.target, "uri")
                .ok_or_else(|| "missing library URI".to_string())?;
            match (server.library_lookup)(uri.trim(), 1_000) {
                Ok(tracks) => {
                    remember_search_uris(&server.recent_search_uris, &tracks);
                    let json = serde_json::to_string(&tracks)
                        .map_err(|error| format!("could not serialize lookup results: {error}"))?;
                    respond_json(stream, 200, "OK", &json)
                }
                Err(error) => respond_text(stream, 502, "Bad Gateway", &format!("{error}\n")),
            }
        }
        ("GET", "/api/artwork") => respond_artwork(stream, playback_status),
        ("GET", "/app.js") => respond_script(stream, 200, "OK", APP_JS),
        ("GET", "/identity.js") => respond_script(stream, 200, "OK", IDENTITY_JS),
        ("GET", "/settings.js") => respond_script(stream, 200, "OK", SETTINGS_JS),
        ("GET", "/pwa.js") => respond_script(stream, 200, "OK", PWA_JS),
        ("GET", "/service-worker.js") => respond_service_worker(stream),
        ("GET", "/manifest.webmanifest") => respond(
            stream,
            200,
            "OK",
            "application/manifest+json; charset=utf-8",
            WEB_MANIFEST,
            &[],
        ),
        ("GET", "/icon-192.png") => respond_app_icon(stream, 192),
        ("GET", "/icon-512.png") => respond_app_icon(stream, 512),
        ("POST", "/settings") => {
            match save_settings_request(&request, settings_path, csrf_token, updates) {
                Ok(true) => {
                    if let Ok(mut sessions) = server.settings_sessions.lock() {
                        sessions.clear();
                    }
                    respond_session_redirect(stream, "/login?changed=1", None)
                }
                Ok(false) => respond_redirect(stream, "/settings?saved=settings"),
                Err(error) => respond_text(
                    stream,
                    400,
                    "Bad Request",
                    &format!("Settings were not applied: {error}\n"),
                ),
            }
        }
        ("POST", "/visualizer-delay") => {
            match save_visualizer_delay_request(&request, settings_path, csrf_token, updates) {
                Ok(()) => respond_json(stream, 202, "Accepted", "{\"ok\":true}"),
                Err(error) => respond_text(
                    stream,
                    400,
                    "Bad Request",
                    &format!("Visualizer delay was not applied: {error}\n"),
                ),
            }
        }
        ("POST", "/spotify-visualizer-delay") => {
            match save_spotify_visualizer_delay_request(
                &request,
                settings_path,
                csrf_token,
                updates,
            ) {
                Ok(()) => respond_json(stream, 202, "Accepted", "{\"ok\":true}"),
                Err(error) => respond_text(
                    stream,
                    400,
                    "Bad Request",
                    &format!("Spotify visualizer delay was not applied: {error}\n"),
                ),
            }
        }
        ("POST", "/carousel-speed") => {
            match save_carousel_speed_request(&request, settings_path, csrf_token, updates) {
                Ok(()) => respond_json(stream, 202, "Accepted", "{\"ok\":true}"),
                Err(error) => respond_text(
                    stream,
                    400,
                    "Bad Request",
                    &format!("Carousel speed was not applied: {error}\n"),
                ),
            }
        }
        ("POST", "/playlists") => {
            match save_playlists_request(&request, playlists_path, csrf_token, updates) {
                Ok(()) => respond_redirect(stream, "/settings?saved=playlists"),
                Err(error) => respond_text(
                    stream,
                    400,
                    "Bad Request",
                    &format!("Playlists were not applied: {error}\n"),
                ),
            }
        }
        ("POST", "/playlists/play") => {
            match send_playlist_play_request(&request, playlists_path, csrf_token, updates) {
                Ok(()) if accepts_json(&request) => {
                    respond_json(stream, 202, "Accepted", "{\"ok\":true}")
                }
                Ok(()) => respond_redirect(stream, "/?sent=playlist"),
                Err(error) => respond_text(
                    stream,
                    400,
                    "Bad Request",
                    &format!("Playlist was not started: {error}\n"),
                ),
            }
        }
        ("POST", "/library/play") => {
            match send_catalog_playlist_request(&request, csrf_token, updates, playlist_catalog) {
                Ok(()) if accepts_json(&request) => {
                    respond_json(stream, 202, "Accepted", "{\"ok\":true}")
                }
                Ok(()) => respond_redirect(stream, "/?sent=playlist"),
                Err(error) => respond_text(
                    stream,
                    400,
                    "Bad Request",
                    &format!("Playlist was not started: {error}\n"),
                ),
            }
        }
        ("POST", "/playback") => match send_playback_request(&request, csrf_token, updates) {
            Ok(()) if accepts_json(&request) => {
                respond_json(stream, 202, "Accepted", "{\"ok\":true}")
            }
            Ok(()) => respond_redirect(stream, "/?sent=playback"),
            Err(error) => respond_text(
                stream,
                400,
                "Bad Request",
                &format!("Playback command was rejected: {error}\n"),
            ),
        },
        ("POST", "/queue") => {
            let guest_policy = guest_restricted.then_some(GuestQueuePolicy {
                client_id,
                cooldown: Duration::from_secs(access_settings.guest_queue_cooldown_seconds),
                attempts: &server.guest_queue_attempts,
            });
            match send_queue_request(
                &request,
                csrf_token,
                updates,
                &server.recent_search_uris,
                &server.queue,
                &server.queue_reservations,
                guest_policy,
            ) {
                Ok(()) if accepts_json(&request) => {
                    respond_json(stream, 202, "Accepted", "{\"ok\":true}")
                }
                Ok(()) => respond_redirect(stream, "/?sent=queue"),
                Err(error) => {
                    let rate_limited = error.starts_with("guest queue cooldown active");
                    let duplicate = error.starts_with("track is already queued");
                    respond_text(
                        stream,
                        if rate_limited {
                            429
                        } else if duplicate {
                            409
                        } else {
                            400
                        },
                        if rate_limited {
                            "Too Many Requests"
                        } else if duplicate {
                            "Conflict"
                        } else {
                            "Bad Request"
                        },
                        &format!("Queue command was rejected: {error}\n"),
                    )
                }
            }
        }
        ("POST", "/queue/edit") => match send_queue_edit_request(&request, csrf_token, updates) {
            Ok(()) => respond_json(stream, 202, "Accepted", "{\"ok\":true}"),
            Err(error) => respond_text(
                stream,
                400,
                "Bad Request",
                &format!("Queue edit was rejected: {error}\n"),
            ),
        },
        ("POST", "/queue/vote") => match send_queue_vote_request(
            &request,
            csrf_token,
            updates,
            &server.queue,
            &server.queue_voters,
            client_id,
        ) {
            Ok(()) => respond_json(stream, 202, "Accepted", "{\"ok\":true}"),
            Err(error) => respond_text(
                stream,
                409,
                "Conflict",
                &format!("Queue vote was rejected: {error}\n"),
            ),
        },
        _ => respond_text(stream, 404, "Not Found", "Not found\n"),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LoginError {
    Rejected,
    RateLimited,
    Unavailable(String),
}

#[derive(Debug, Clone, Serialize)]
struct WebDiagnostics {
    app_uptime_seconds: u64,
    system_uptime_seconds: u64,
    cpu_temperature_c: Option<f64>,
    source: String,
    playback_online: bool,
    mopidy_online: bool,
    spotifyd_available: bool,
    now_playing_style: String,
    queue_tracks: usize,
    history_entries: usize,
    artwork_cache_files: usize,
    artwork_cache_bytes: u64,
    persistent_queue_file: bool,
}

#[derive(Serialize)]
struct WebLiveSnapshot {
    playback: WebPlaybackStatus,
    queue: Vec<MopidyQueueTrack>,
}

#[derive(Serialize)]
struct PublicStatusTrack {
    title: String,
    artist: String,
    album: String,
    duration_ms: u64,
}

#[derive(Serialize)]
struct PublicStatusNextTrack {
    title: String,
    artist: String,
}

#[derive(Serialize)]
struct PublicStatusServices {
    active_source_online: bool,
    mopidy_online: bool,
    spotify_connect_available: bool,
}

#[derive(Serialize)]
struct PublicStatusDisplay {
    screensaver_active: bool,
    carousel_cover_count: usize,
    carousel_speed_px_per_second: f64,
}

#[derive(Serialize)]
struct PublicStatusSystem {
    orpheus_version: &'static str,
    app_uptime_seconds: u64,
    host_uptime_seconds: u64,
    cpu_temperature_c: Option<f64>,
}

#[derive(Serialize)]
struct PublicStatusSummary {
    schema_version: u8,
    timestamp_ms: u64,
    healthy: bool,
    state: &'static str,
    source: String,
    track: Option<PublicStatusTrack>,
    position_ms: u64,
    progress_percent: Option<f64>,
    volume_percent: Option<u8>,
    max_volume_percent: u8,
    clients: usize,
    queue_length: usize,
    queue_upcoming: usize,
    queue_duration_ms: u64,
    queue_votes: u64,
    history_entries: usize,
    next_track: Option<PublicStatusNextTrack>,
    services: PublicStatusServices,
    display: PublicStatusDisplay,
    system: PublicStatusSystem,
}

fn load_access_settings(path: &Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| toml::from_str(&contents).ok())
        .unwrap_or_default()
}

fn seconds_to_milliseconds(seconds: f64) -> u64 {
    if seconds.is_finite() && seconds > 0.0 {
        (seconds * 1_000.0).round() as u64
    } else {
        0
    }
}

fn system_uptime_seconds() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|value| value.split_whitespace().next()?.parse::<f64>().ok())
        .map(|seconds| seconds.max(0.0) as u64)
        .unwrap_or_default()
}

fn cpu_temperature_c() -> Option<f64> {
    std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .map(|millidegrees| millidegrees / 1_000.0)
}

fn collect_public_status(server: &WebServerState) -> PublicStatusSummary {
    let playback = server
        .playback_status
        .lock()
        .map(|status| status.clone())
        .unwrap_or_default();
    let queue = server
        .queue
        .lock()
        .map(|queue| queue.clone())
        .unwrap_or_default();
    let duration_ms = seconds_to_milliseconds(playback.duration_seconds);
    let position_ms = seconds_to_milliseconds(playback.position_seconds);
    let has_track = !playback.title.trim().is_empty() && playback.title != "No track playing";
    let state = if !playback.online {
        "offline"
    } else if playback.is_playing {
        "playing"
    } else if has_track {
        "paused"
    } else {
        "stopped"
    };
    let progress_percent = (duration_ms > 0).then(|| {
        let percent = position_ms as f64 / duration_ms as f64 * 100.0;
        (percent.clamp(0.0, 100.0) * 100.0).round() / 100.0
    });
    let queue_duration_ms = queue.iter().fold(0_u64, |total, track| {
        total.saturating_add(track.duration_ms)
    });
    let queue_votes = queue.iter().fold(0_u64, |total, track| {
        total.saturating_add(track.votes as u64)
    });

    PublicStatusSummary {
        schema_version: 1,
        timestamp_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default(),
        healthy: playback.online,
        state,
        source: playback.source.clone(),
        track: has_track.then(|| PublicStatusTrack {
            title: playback.title,
            artist: playback.artist,
            album: playback.album,
            duration_ms,
        }),
        position_ms,
        progress_percent,
        volume_percent: playback.volume,
        max_volume_percent: playback.max_volume,
        clients: server.active_clients.load(Ordering::Relaxed),
        queue_length: queue.len(),
        queue_upcoming: queue.iter().filter(|track| !track.current).count(),
        queue_duration_ms,
        queue_votes,
        history_entries: server
            .history
            .lock()
            .map(|history| history.len())
            .unwrap_or_default(),
        next_track: playback.next_title.map(|title| PublicStatusNextTrack {
            title,
            artist: playback.next_artist.unwrap_or_default(),
        }),
        services: PublicStatusServices {
            active_source_online: playback.online,
            mopidy_online: playback.mopidy_online,
            spotify_connect_available: playback.spotifyd_available,
        },
        display: PublicStatusDisplay {
            screensaver_active: playback.screensaver_active,
            carousel_cover_count: playback.carousel_cover_count,
            carousel_speed_px_per_second: playback.carousel_speed,
        },
        system: PublicStatusSystem {
            orpheus_version: env!("CARGO_PKG_VERSION"),
            app_uptime_seconds: server.started_at.elapsed().as_secs(),
            host_uptime_seconds: system_uptime_seconds(),
            cpu_temperature_c: cpu_temperature_c(),
        },
    }
}

fn collect_diagnostics(server: &WebServerState) -> WebDiagnostics {
    let playback = server
        .playback_status
        .lock()
        .map(|status| status.clone())
        .unwrap_or_default();
    let system_uptime_seconds = system_uptime_seconds();
    let cpu_temperature_c = cpu_temperature_c();
    let (artwork_cache_files, artwork_cache_bytes) = directory_usage(Path::new("/tmp/orpheus_art"));
    let settings = load_access_settings(&server.settings_path);
    WebDiagnostics {
        app_uptime_seconds: server.started_at.elapsed().as_secs(),
        system_uptime_seconds,
        cpu_temperature_c,
        source: playback.source,
        playback_online: playback.online,
        mopidy_online: playback.mopidy_online,
        spotifyd_available: playback.spotifyd_available,
        now_playing_style: settings.now_playing_style.label().to_string(),
        queue_tracks: server.queue.lock().map(|queue| queue.len()).unwrap_or(0),
        history_entries: server
            .history
            .lock()
            .map(|history| history.len())
            .unwrap_or(0),
        artwork_cache_files,
        artwork_cache_bytes,
        persistent_queue_file: server
            .settings_path
            .parent()
            .is_some_and(|parent| parent.join("queue-state.toml").exists()),
    }
}

fn directory_usage(path: &Path) -> (usize, u64) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return (0, 0);
    };
    entries.filter_map(Result::ok).fold((0, 0), |total, entry| {
        let path = entry.path();
        if path.is_dir() {
            let nested = directory_usage(&path);
            (total.0 + nested.0, total.1 + nested.1)
        } else {
            let bytes = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            (total.0 + 1, total.1 + bytes)
        }
    })
}

fn settings_route_requires_auth(method: &str, path: &str) -> bool {
    matches!(
        (method, path),
        ("GET", "/settings")
            | ("POST", "/settings")
            | ("POST", "/visualizer-delay")
            | ("POST", "/spotify-visualizer-delay")
            | ("POST", "/carousel-speed")
            | ("POST", "/playlists")
            | ("POST", "/logout")
            | ("GET", "/history")
            | ("GET", "/diagnostics")
            | ("GET", "/api/history")
            | ("GET", "/api/diagnostics")
    )
}

fn guest_playback_request_is_allowed(request: &HttpRequest) -> bool {
    parse_form(&request.body)
        .ok()
        .and_then(|form| form.get("action").cloned())
        .is_some_and(|action| matches!(action.as_str(), "toggle" | "volume"))
}

fn request_session_token(request: &HttpRequest) -> Option<&str> {
    request
        .headers
        .get("cookie")?
        .split(';')
        .find_map(|cookie| {
            let (name, value) = cookie.trim().split_once('=')?;
            (name == "orpheus_settings").then_some(value)
        })
}

fn settings_request_is_authenticated(
    request: &HttpRequest,
    sessions: &Arc<Mutex<HashMap<String, Instant>>>,
) -> bool {
    let Some(token) = request_session_token(request) else {
        return false;
    };
    let Ok(mut sessions) = sessions.lock() else {
        return false;
    };
    sessions.retain(|_, created| created.elapsed() < SETTINGS_SESSION_TTL);
    sessions.contains_key(token)
}

fn login_request(
    request: &HttpRequest,
    settings_path: &Path,
    csrf_token: &str,
    sessions: &Arc<Mutex<HashMap<String, Instant>>>,
    failures: &Arc<Mutex<VecDeque<Instant>>>,
) -> Result<String, LoginError> {
    ensure_form_content_type(request).map_err(|_| LoginError::Rejected)?;
    let form = parse_form(&request.body).map_err(|_| LoginError::Rejected)?;
    validate_csrf(&form, csrf_token).map_err(|_| LoginError::Rejected)?;
    let now = Instant::now();
    {
        let mut failures = failures
            .lock()
            .map_err(|_| LoginError::Unavailable("login limiter is unavailable".to_string()))?;
        while failures
            .front()
            .is_some_and(|attempt| now.duration_since(*attempt) >= LOGIN_FAILURE_WINDOW)
        {
            failures.pop_front();
        }
        if failures.len() >= MAX_LOGIN_FAILURES {
            return Err(LoginError::RateLimited);
        }
    }

    let supplied = form.get("password").map(String::as_str).unwrap_or_default();
    let expected = Settings::load(&settings_path.to_string_lossy()).web_settings_password;
    if expected.is_empty() {
        return Err(LoginError::Unavailable(
            "no configuration password has been initialized".to_string(),
        ));
    }
    if !constant_time_equal(supplied.as_bytes(), expected.as_bytes()) {
        if let Ok(mut failures) = failures.lock() {
            failures.push_back(now);
        }
        return Err(LoginError::Rejected);
    }

    let session = generate_csrf_token();
    sessions
        .lock()
        .map_err(|_| LoginError::Unavailable("session store is unavailable".to_string()))?
        .insert(session.clone(), now);
    if let Ok(mut failures) = failures.lock() {
        failures.clear();
    }
    Ok(session)
}

fn logout_request(
    request: &HttpRequest,
    csrf_token: &str,
    sessions: &Arc<Mutex<HashMap<String, Instant>>>,
) -> Result<(), String> {
    ensure_form_content_type(request)?;
    let form = parse_form(&request.body)?;
    validate_csrf(&form, csrf_token)?;
    if let Some(token) = request_session_token(request)
        && let Ok(mut sessions) = sessions.lock()
    {
        sessions.remove(token);
    }
    Ok(())
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let length = left.len().max(right.len());
    let mut different = left.len() ^ right.len();
    for index in 0..length {
        let left_byte = left.get(index).copied().unwrap_or_default();
        let right_byte = right.get(index).copied().unwrap_or_default();
        different |= usize::from(left_byte ^ right_byte);
    }
    different == 0
}

fn save_settings_request(
    request: &HttpRequest,
    settings_path: &Path,
    csrf_token: &str,
    updates: &Sender<WebConfigUpdate>,
) -> Result<bool, String> {
    ensure_form_content_type(request)?;
    let form = parse_form(&request.body)?;
    validate_csrf(&form, csrf_token)?;
    let current = Settings::load(&settings_path.to_string_lossy());
    let previous_password = current.web_settings_password.clone();
    let settings = settings_from_form(current, &form)?;
    let password_changed = settings.web_settings_password != previous_password;
    settings.save(settings_path)?;
    updates
        .send(WebConfigUpdate::Settings(settings))
        .map_err(|_| "display process is no longer accepting updates".to_string())?;
    Ok(password_changed)
}

fn save_visualizer_delay_request(
    request: &HttpRequest,
    settings_path: &Path,
    csrf_token: &str,
    updates: &Sender<WebConfigUpdate>,
) -> Result<(), String> {
    ensure_form_content_type(request)?;
    let form = parse_form(&request.body)?;
    validate_csrf(&form, csrf_token)?;
    let mut settings = Settings::load(&settings_path.to_string_lossy());
    settings.visualizer_delay_ms = integer(&form, "visualizer_delay_ms", 0, 1_500)?;
    settings.normalize();
    settings.save(settings_path)?;
    updates
        .send(WebConfigUpdate::Settings(settings))
        .map_err(|_| "display process is no longer accepting updates".to_string())
}

fn save_spotify_visualizer_delay_request(
    request: &HttpRequest,
    settings_path: &Path,
    csrf_token: &str,
    updates: &Sender<WebConfigUpdate>,
) -> Result<(), String> {
    ensure_form_content_type(request)?;
    let form = parse_form(&request.body)?;
    validate_csrf(&form, csrf_token)?;
    let mut settings = Settings::load(&settings_path.to_string_lossy());
    settings.spotify_visualizer_extra_delay_ms =
        integer(&form, "spotify_visualizer_extra_delay_ms", 0, 1_500)?;
    settings.normalize();
    settings.save(settings_path)?;
    updates
        .send(WebConfigUpdate::Settings(settings))
        .map_err(|_| "display process is no longer accepting updates".to_string())
}

fn save_carousel_speed_request(
    request: &HttpRequest,
    settings_path: &Path,
    csrf_token: &str,
    updates: &Sender<WebConfigUpdate>,
) -> Result<(), String> {
    ensure_form_content_type(request)?;
    let form = parse_form(&request.body)?;
    validate_csrf(&form, csrf_token)?;
    let mut settings = Settings::load(&settings_path.to_string_lossy());
    settings.carousel_speed = decimal(&form, "carousel_speed", 0.0, 80.0)?;
    settings.normalize();
    settings.save(settings_path)?;
    updates
        .send(WebConfigUpdate::Settings(settings))
        .map_err(|_| "display process is no longer accepting updates".to_string())
}

fn save_playlists_request(
    request: &HttpRequest,
    playlists_path: &Path,
    csrf_token: &str,
    updates: &Sender<WebConfigUpdate>,
) -> Result<(), String> {
    ensure_form_content_type(request)?;
    let form = parse_form(&request.body)?;
    validate_csrf(&form, csrf_token)?;
    let config = playlists_from_form(&form)?;
    config.save(playlists_path)?;
    updates
        .send(WebConfigUpdate::Playlists(config))
        .map_err(|_| "display process is no longer accepting updates".to_string())?;
    Ok(())
}

fn send_playlist_play_request(
    request: &HttpRequest,
    playlists_path: &Path,
    csrf_token: &str,
    updates: &Sender<WebConfigUpdate>,
) -> Result<(), String> {
    ensure_form_content_type(request)?;
    let form = parse_form(&request.body)?;
    validate_csrf(&form, csrf_token)?;
    let index = required(&form, "playlist_index")?
        .parse::<usize>()
        .map_err(|_| "'playlist_index' must be a valid playlist number".to_string())?;
    let config = Config::load(&playlists_path.to_string_lossy());
    let entry = config
        .playlists
        .get(index)
        .cloned()
        .ok_or_else(|| "playlist does not exist in the saved configuration".to_string())?;
    updates
        .send(WebConfigUpdate::PlayPlaylist(entry))
        .map_err(|_| "display process is no longer accepting updates".to_string())
}

fn send_catalog_playlist_request(
    request: &HttpRequest,
    csrf_token: &str,
    updates: &Sender<WebConfigUpdate>,
    playlist_catalog: &Arc<Mutex<Vec<WebPlaylist>>>,
) -> Result<(), String> {
    ensure_form_content_type(request)?;
    let form = parse_form(&request.body)?;
    validate_csrf(&form, csrf_token)?;
    let uri = required(&form, "playlist_uri")?;
    let playlist = playlist_catalog
        .lock()
        .map_err(|_| "playlist catalog is unavailable".to_string())?
        .iter()
        .find(|playlist| playlist.uri == uri)
        .cloned()
        .ok_or_else(|| "playlist is not in the current Mopidy catalog".to_string())?;
    updates
        .send(WebConfigUpdate::PlayPlaylist(PlaylistEntry {
            name: playlist.name,
            uri: playlist.uri,
            art_uri: None,
        }))
        .map_err(|_| "display process is no longer accepting updates".to_string())
}

fn send_playback_request(
    request: &HttpRequest,
    csrf_token: &str,
    updates: &Sender<WebConfigUpdate>,
) -> Result<(), String> {
    ensure_form_content_type(request)?;
    let form = parse_form(&request.body)?;
    validate_csrf(&form, csrf_token)?;
    let action = match required(&form, "action")? {
        "previous" => WebPlaybackAction::Previous,
        "toggle" => WebPlaybackAction::TogglePlayPause,
        "next" => WebPlaybackAction::Next,
        "seek" => WebPlaybackAction::Seek(integer(&form, "position_seconds", 0, 7 * 24 * 60 * 60)?),
        "volume" => WebPlaybackAction::SetVolume(integer(&form, "volume", 0, 100)? as u8),
        "screensaver" => WebPlaybackAction::ToggleScreensaver,
        _ => return Err("unknown playback action".to_string()),
    };
    updates
        .send(WebConfigUpdate::Playback(action))
        .map_err(|_| "display process is no longer accepting updates".to_string())
}

fn send_queue_request(
    request: &HttpRequest,
    csrf_token: &str,
    updates: &Sender<WebConfigUpdate>,
    recent_search_uris: &Arc<Mutex<VecDeque<String>>>,
    queue: &Arc<Mutex<Vec<MopidyQueueTrack>>>,
    reservations: &Arc<Mutex<HashMap<String, Instant>>>,
    guest_policy: Option<GuestQueuePolicy<'_>>,
) -> Result<(), String> {
    ensure_form_content_type(request)?;
    let form = parse_form(&request.body)?;
    validate_csrf(&form, csrf_token)?;
    let uri = required(&form, "track_uri")?;
    if !uri.starts_with("jellyfin:track:") || uri.len() > 512 {
        return Err("invalid Jellyfin track URI".to_string());
    }
    let is_recent_result = recent_search_uris
        .lock()
        .map_err(|_| "search result cache is unavailable".to_string())?
        .iter()
        .any(|candidate| candidate == uri);
    if !is_recent_result {
        return Err("track is not from a recent Jellyfin search".to_string());
    }
    let placement = match required(&form, "placement")? {
        "end" => QueuePlacement::End,
        "next" => QueuePlacement::Next,
        "now" => QueuePlacement::Now,
        _ => return Err("unknown queue placement".to_string()),
    };
    let requested_by = Some(validated_requester_name(&form)?);
    reject_duplicate_queue_uri(uri, queue, reservations)?;
    if let Some(policy) = guest_policy.as_ref() {
        if placement != QueuePlacement::End {
            return Err("guests can only add songs to the end of the queue".to_string());
        }
        enforce_guest_queue_cooldown(policy)?;
    }
    reserve_queue_uri(uri, reservations)?;
    if let Err(error) = updates.send(WebConfigUpdate::QueueTrack {
        uri: uri.to_string(),
        placement,
        requested_by,
    }) {
        if let Ok(mut reservations) = reservations.lock() {
            reservations.remove(uri);
        }
        return Err(format!(
            "display process is no longer accepting updates: {error}"
        ));
    }
    Ok(())
}

struct GuestQueuePolicy<'a> {
    client_id: &'a str,
    cooldown: Duration,
    attempts: &'a Arc<Mutex<HashMap<String, Instant>>>,
}

fn enforce_guest_queue_cooldown(policy: &GuestQueuePolicy<'_>) -> Result<(), String> {
    let now = Instant::now();
    let mut attempts = policy
        .attempts
        .lock()
        .map_err(|_| "guest queue limiter is unavailable".to_string())?;
    attempts.retain(|_, queued_at| now.duration_since(*queued_at) < policy.cooldown);
    if let Some(queued_at) = attempts.get(policy.client_id) {
        let remaining = policy
            .cooldown
            .saturating_sub(now.duration_since(*queued_at))
            .as_secs()
            .max(1);
        return Err(format!(
            "guest queue cooldown active; try again in {remaining} seconds"
        ));
    }
    attempts.insert(policy.client_id.to_string(), now);
    Ok(())
}

fn validated_requester_name(form: &HashMap<String, String>) -> Result<String, String> {
    let name = form
        .get("requester_name")
        .map(|name| name.trim())
        .unwrap_or_default();
    if name.is_empty() {
        return Err("enter your name before adding a song".to_string());
    }
    if name.chars().count() > 32 || name.chars().any(char::is_control) {
        return Err("requester name must be 1 to 32 printable characters".to_string());
    }
    Ok(name.to_string())
}

fn reject_duplicate_queue_uri(
    uri: &str,
    queue: &Arc<Mutex<Vec<MopidyQueueTrack>>>,
    reservations: &Arc<Mutex<HashMap<String, Instant>>>,
) -> Result<(), String> {
    if queue
        .lock()
        .map_err(|_| "queue is unavailable".to_string())?
        .iter()
        .any(|track| track.uri == uri)
    {
        return Err("track is already queued".to_string());
    }
    let now = Instant::now();
    let mut reservations = reservations
        .lock()
        .map_err(|_| "queue reservation store is unavailable".to_string())?;
    reservations.retain(|_, added| now.duration_since(*added) < Duration::from_secs(10));
    if reservations.contains_key(uri) {
        return Err("track is already queued by another client".to_string());
    }
    Ok(())
}

fn reserve_queue_uri(
    uri: &str,
    reservations: &Arc<Mutex<HashMap<String, Instant>>>,
) -> Result<(), String> {
    let mut reservations = reservations
        .lock()
        .map_err(|_| "queue reservation store is unavailable".to_string())?;
    if reservations.contains_key(uri) {
        return Err("track is already queued by another client".to_string());
    }
    reservations.insert(uri.to_string(), Instant::now());
    Ok(())
}

fn send_queue_edit_request(
    request: &HttpRequest,
    csrf_token: &str,
    updates: &Sender<WebConfigUpdate>,
) -> Result<(), String> {
    ensure_form_content_type(request)?;
    let form = parse_form(&request.body)?;
    validate_csrf(&form, csrf_token)?;
    let action = required(&form, "action")?;
    let queue_action = if action == "clear" {
        WebQueueAction::Clear
    } else {
        let tlid = integer(&form, "tlid", 1, u64::MAX)?;
        match action {
            "remove" => WebQueueAction::Remove(tlid),
            "up" => WebQueueAction::MoveUp(tlid),
            "down" => WebQueueAction::MoveDown(tlid),
            "play" => WebQueueAction::Play(tlid),
            _ => return Err("unknown queue edit action".to_string()),
        }
    };
    updates
        .send(WebConfigUpdate::QueueEdit(queue_action))
        .map_err(|_| "display process is no longer accepting updates".to_string())
}

fn send_queue_vote_request(
    request: &HttpRequest,
    csrf_token: &str,
    updates: &Sender<WebConfigUpdate>,
    queue: &Arc<Mutex<Vec<MopidyQueueTrack>>>,
    voters: &Arc<Mutex<HashMap<u64, BTreeSet<String>>>>,
    client_id: &str,
) -> Result<(), String> {
    ensure_form_content_type(request)?;
    let form = parse_form(&request.body)?;
    validate_csrf(&form, csrf_token)?;
    let tlid = integer(&form, "tlid", 1, u64::MAX)?;
    let queue = queue
        .lock()
        .map_err(|_| "queue is unavailable".to_string())?;
    let index = queue
        .iter()
        .position(|track| track.tlid == tlid)
        .ok_or_else(|| "track is no longer queued".to_string())?;
    let upcoming_start = queue
        .iter()
        .position(|track| track.current)
        .map(|current| current + 1)
        .unwrap_or(0);
    if index < upcoming_start {
        return Err("only upcoming tracks can receive votes".to_string());
    }
    let active: BTreeSet<u64> = queue.iter().map(|track| track.tlid).collect();
    drop(queue);

    let mut voters = voters
        .lock()
        .map_err(|_| "queue voter store is unavailable".to_string())?;
    voters.retain(|tlid, _| active.contains(tlid));
    let track_voters = voters.entry(tlid).or_default();
    if !track_voters.insert(client_id.to_string()) {
        return Err("this client has already voted for that track".to_string());
    }
    if updates.send(WebConfigUpdate::QueueVote(tlid)).is_err() {
        track_voters.remove(client_id);
        return Err("display process is no longer accepting updates".to_string());
    }
    Ok(())
}

fn remember_search_uris(
    recent_search_uris: &Arc<Mutex<VecDeque<String>>>,
    tracks: &[MopidySearchTrack],
) {
    let Ok(mut recent) = recent_search_uris.lock() else {
        return;
    };
    for track in tracks {
        if let Some(existing) = recent.iter().position(|uri| uri == &track.uri) {
            recent.remove(existing);
        }
        recent.push_back(track.uri.clone());
    }
    while recent.len() > MAX_RECENT_SEARCH_URIS {
        recent.pop_front();
    }
}

fn read_request<R: Read>(stream: &mut R) -> Result<HttpRequest, String> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    let mut expected_length = None;

    loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err("request is too large".to_string());
        }

        if expected_length.is_none()
            && let Some(header_end) = find_bytes(&bytes, b"\r\n\r\n")
        {
            let headers = std::str::from_utf8(&bytes[..header_end])
                .map_err(|_| "request headers are not UTF-8".to_string())?;
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            expected_length = Some(header_end + 4 + content_length);
        }

        if expected_length.is_some_and(|length| bytes.len() >= length) {
            break;
        }
    }

    let header_end =
        find_bytes(&bytes, b"\r\n\r\n").ok_or_else(|| "incomplete HTTP headers".to_string())?;
    let header_text = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| "request headers are not UTF-8".to_string())?;
    let mut lines = header_text.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| "missing HTTP request line".to_string())?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| "missing HTTP method".to_string())?
        .to_string();
    let target = request_parts
        .next()
        .ok_or_else(|| "missing HTTP target".to_string())?
        .to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let body = bytes[header_end + 4..].to_vec();

    Ok(HttpRequest {
        method,
        target,
        headers,
        body,
    })
}

fn ensure_form_content_type(request: &HttpRequest) -> Result<(), String> {
    let content_type = request
        .headers
        .get("content-type")
        .map(String::as_str)
        .unwrap_or_default();
    if content_type.starts_with("application/x-www-form-urlencoded") {
        Ok(())
    } else {
        Err("unsupported form content type".to_string())
    }
}

fn parse_form(body: &[u8]) -> Result<HashMap<String, String>, String> {
    std::str::from_utf8(body)
        .map_err(|_| "form body is not UTF-8".to_string())
        .map(|body| {
            url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect()
        })
}

fn validate_csrf(form: &HashMap<String, String>, expected: &str) -> Result<(), String> {
    if form
        .get("csrf")
        .is_some_and(|provided| provided == expected)
    {
        Ok(())
    } else {
        Err("invalid or missing form token".to_string())
    }
}

fn accepts_json(request: &HttpRequest) -> bool {
    request.headers.get("accept").is_some_and(|accept| {
        accept
            .split(',')
            .any(|value| value.trim() == "application/json")
    })
}

fn respond_artwork<W: Write>(
    stream: &mut W,
    playback_status: &Arc<Mutex<WebPlaybackStatus>>,
) -> Result<(), String> {
    let artwork_path = playback_status
        .lock()
        .ok()
        .and_then(|status| status.artwork_path.clone());
    let Some(artwork_path) = artwork_path else {
        return respond_text(stream, 404, "Not Found", "No artwork available\n");
    };
    let path = Path::new(&artwork_path);
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return respond_text(stream, 404, "Not Found", "No artwork available\n"),
    };
    if metadata.len() > MAX_ARTWORK_BYTES {
        return respond_text(stream, 413, "Content Too Large", "Artwork is too large\n");
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return respond_text(stream, 404, "Not Found", "No artwork available\n"),
    };
    let Some(content_type) = image_content_type(&bytes) else {
        return respond_text(
            stream,
            415,
            "Unsupported Media Type",
            "Unsupported artwork format\n",
        );
    };
    respond_bytes(stream, 200, "OK", content_type, &bytes, &[])
}

fn image_content_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

fn settings_from_form(
    mut settings: Settings,
    form: &HashMap<String, String>,
) -> Result<Settings, String> {
    settings.display_brightness = percentage(form, "display_brightness", 10.0, 100.0)?;
    settings.dim_timeout_seconds = integer(form, "dim_timeout_seconds", 0, 86_400)?;
    settings.dim_brightness = percentage(form, "dim_brightness", 0.0, 100.0)?;
    settings.screensaver_brightness = percentage(form, "screensaver_brightness", 5.0, 100.0)?;
    settings.volume_step = integer(form, "volume_step", 1, 25)? as u8;
    if form.contains_key("max_volume") {
        settings.max_volume = integer(form, "max_volume", 10, 100)? as u8;
    }
    if form.contains_key("hardware_volume_ceiling_db") {
        settings.hardware_volume_ceiling_db =
            decimal(form, "hardware_volume_ceiling_db", -40.0, 0.0)?;
    }
    settings.visualizer_delay_ms = integer(form, "visualizer_delay_ms", 0, 1_500)?;
    if form.contains_key("spotify_visualizer_extra_delay_ms") {
        settings.spotify_visualizer_extra_delay_ms =
            integer(form, "spotify_visualizer_extra_delay_ms", 0, 1_500)?;
    }
    settings.carousel_speed = decimal(form, "carousel_speed", 0.0, 80.0)?;
    settings.shuffle_screensaver_art = form.contains_key("shuffle_screensaver_art");
    settings.shuffle_playlists = form.contains_key("shuffle_playlists");
    settings.guest_mode_enabled = form.contains_key("guest_mode_enabled");
    if form.contains_key("guest_queue_cooldown_seconds") {
        settings.guest_queue_cooldown_seconds =
            integer(form, "guest_queue_cooldown_seconds", 30, 3_600)?;
    }

    match required(form, "idle_mode")? {
        "dim" => {
            settings.screensaver_enabled = false;
            settings.screensaver_heavy_dim = false;
        }
        "screensaver" => {
            settings.screensaver_enabled = true;
            settings.screensaver_heavy_dim = false;
        }
        "screensaver_dim" => {
            settings.screensaver_enabled = true;
            settings.screensaver_heavy_dim = true;
        }
        _ => return Err("invalid idle display mode".to_string()),
    }
    settings.screensaver_style = match required(form, "screensaver_style")? {
        "albums" => ScreensaverStyle::Albums,
        "clock" => ScreensaverStyle::Clock,
        _ => return Err("invalid screensaver style".to_string()),
    };
    settings.now_playing_style = match required(form, "now_playing_style")? {
        "artwork" => NowPlayingStyle::Artwork,
        "visualizer" => NowPlayingStyle::Visualizer,
        "track_info" => NowPlayingStyle::TrackInfo,
        _ => return Err("invalid now-playing style".to_string()),
    };

    settings.weather_location = required(form, "weather_location")?.trim().to_string();
    if settings.weather_location.is_empty() || settings.weather_location.chars().count() > 64 {
        return Err("weather location must be between 1 and 64 characters".to_string());
    }
    settings.weather_latitude = decimal(form, "weather_latitude", -90.0, 90.0)?;
    settings.weather_longitude = decimal(form, "weather_longitude", -180.0, 180.0)?;
    settings.morning_mode_enabled = form.contains_key("morning_mode_enabled");
    settings.morning_start_hour = integer(form, "morning_start_hour", 0, 23)? as u8;
    settings.morning_end_hour = integer(form, "morning_end_hour", 1, 24)? as u8;
    if settings.morning_start_hour >= settings.morning_end_hour {
        return Err("morning start time must be before its end time".to_string());
    }
    settings.morning_news_feed_url = form
        .get("morning_news_feed_url")
        .map(|value| value.trim())
        .unwrap_or_default()
        .to_string();
    if settings.morning_news_feed_url.len() > 2_048
        || (!settings.morning_news_feed_url.is_empty()
            && !settings.morning_news_feed_url.starts_with("https://")
            && !settings.morning_news_feed_url.starts_with("http://"))
    {
        return Err("morning news feed must be an HTTP(S) URL under 2048 characters".to_string());
    }
    if let Some(password) = form
        .get("new_web_settings_password")
        .map(|password| password.trim())
        .filter(|password| !password.is_empty())
    {
        if !(10..=128).contains(&password.chars().count()) || password.chars().any(char::is_control)
        {
            return Err("new settings password must contain 10 to 128 characters".to_string());
        }
        settings.web_settings_password = password.to_string();
    }
    settings.normalize();
    Ok(settings)
}

fn playlists_from_form(form: &HashMap<String, String>) -> Result<Config, String> {
    let mut indices = BTreeSet::<usize>::new();
    for key in form.keys() {
        for prefix in ["playlist_name_", "playlist_uri_", "playlist_art_"] {
            if let Some(index) = key
                .strip_prefix(prefix)
                .and_then(|value| value.parse().ok())
            {
                indices.insert(index);
            }
        }
    }

    let mut playlists = Vec::new();
    for index in indices {
        let name = form
            .get(&format!("playlist_name_{index}"))
            .map(|value| value.trim())
            .unwrap_or_default();
        let uri = form
            .get(&format!("playlist_uri_{index}"))
            .map(|value| value.trim())
            .unwrap_or_default();
        let art_uri = form
            .get(&format!("playlist_art_{index}"))
            .map(|value| value.trim())
            .unwrap_or_default();

        if name.is_empty() && uri.is_empty() && art_uri.is_empty() {
            continue;
        }
        if name.is_empty() || uri.is_empty() {
            return Err(format!("playlist {} needs both a name and URI", index + 1));
        }
        if name.chars().count() > 100 || uri.chars().count() > 2048 || art_uri.len() > 2048 {
            return Err(format!(
                "playlist {} contains an oversized field",
                index + 1
            ));
        }
        playlists.push(PlaylistEntry {
            name: name.to_string(),
            uri: uri.to_string(),
            art_uri: (!art_uri.is_empty()).then(|| art_uri.to_string()),
        });
    }
    if playlists.len() > 64 {
        return Err("at most 64 playlists are supported".to_string());
    }
    Ok(Config { playlists })
}

fn required<'a>(form: &'a HashMap<String, String>, name: &str) -> Result<&'a str, String> {
    form.get(name)
        .map(String::as_str)
        .ok_or_else(|| format!("missing field '{name}'"))
}

fn decimal(
    form: &HashMap<String, String>,
    name: &str,
    minimum: f64,
    maximum: f64,
) -> Result<f64, String> {
    let value = required(form, name)?
        .parse::<f64>()
        .map_err(|_| format!("'{name}' must be a number"))?;
    if value.is_finite() && (minimum..=maximum).contains(&value) {
        Ok(value)
    } else {
        Err(format!("'{name}' must be between {minimum} and {maximum}"))
    }
}

fn percentage(
    form: &HashMap<String, String>,
    name: &str,
    minimum: f64,
    maximum: f64,
) -> Result<f64, String> {
    Ok(decimal(form, name, minimum, maximum)? / 100.0)
}

fn integer(
    form: &HashMap<String, String>,
    name: &str,
    minimum: u64,
    maximum: u64,
) -> Result<u64, String> {
    let value = required(form, name)?
        .parse::<u64>()
        .map_err(|_| format!("'{name}' must be a whole number"))?;
    if (minimum..=maximum).contains(&value) {
        Ok(value)
    } else {
        Err(format!("'{name}' must be between {minimum} and {maximum}"))
    }
}

const WEB_STYLE: &str = r#"
:root{color-scheme:dark;--bg:#080b12;--surface:rgba(20,25,36,.82);--surface-2:#171d2a;--line:rgba(255,255,255,.09);--text:#f7f8fb;--muted:#929aad;--accent:#8ab4f8;--accent-2:#a78bfa;--good:#73e2a7;--danger:#ff8f9d;--shadow:0 24px 70px rgba(0,0,0,.34)}
*{box-sizing:border-box}html{min-height:100%;background:var(--bg)}body{min-height:100%;margin:0;color:var(--text);font:15px/1.45 Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:radial-gradient(circle at 15% -10%,rgba(99,102,241,.24),transparent 38%),radial-gradient(circle at 100% 8%,rgba(14,165,233,.17),transparent 32%),var(--bg);padding:0 env(safe-area-inset-right) 0 env(safe-area-inset-left)}
body:before{content:"";position:fixed;inset:0;pointer-events:none;background-image:linear-gradient(rgba(255,255,255,.018) 1px,transparent 1px),linear-gradient(90deg,rgba(255,255,255,.018) 1px,transparent 1px);background-size:34px 34px;mask-image:linear-gradient(to bottom,black,transparent 65%)}
.identity-pending,.identity-required{overflow:hidden}.identity-pending main,.identity-required main{pointer-events:none;user-select:none;filter:blur(8px);opacity:.32}.identity-gate{position:fixed;inset:0;z-index:1000;display:grid;place-items:center;padding:22px;background:rgba(3,5,10,.78);backdrop-filter:blur(18px)}.identity-card{width:min(430px,100%);padding:28px;border:1px solid var(--line);border-radius:22px;background:linear-gradient(145deg,rgba(27,34,49,.98),rgba(12,16,24,.99));box-shadow:0 30px 90px rgba(0,0,0,.58)}.identity-card .brand-mark{margin-bottom:18px}.identity-card h2{font-size:25px;margin:5px 0}.identity-card form{margin-top:21px}.identity-card button{width:100%}.identity-card .login-error{min-height:18px}
main{position:relative;width:min(1040px,calc(100% - 32px));margin:0 auto;padding:34px 0 72px}.app-header{display:flex;align-items:center;justify-content:space-between;gap:18px;margin-bottom:24px}.brand{display:flex;align-items:center;gap:13px}.brand-mark{display:grid;place-items:center;width:44px;height:44px;border-radius:14px;background:linear-gradient(145deg,var(--accent),var(--accent-2));box-shadow:0 10px 30px rgba(99,102,241,.25);color:#07101e;font-size:21px;font-weight:900}.brand h1{font-size:24px;line-height:1.1;margin:0;letter-spacing:-.03em}.brand p{margin:4px 0 0}.eyebrow{color:var(--accent);font-size:11px;font-weight:800;letter-spacing:.12em;text-transform:uppercase}h2{margin:0;font-size:18px;letter-spacing:-.015em}.card>h2{margin-bottom:18px}p{color:var(--muted);margin:5px 0}a{color:var(--accent)}
.tabs{display:flex;gap:5px;padding:5px;border:1px solid var(--line);border-radius:14px;background:rgba(8,11,18,.58);backdrop-filter:blur(18px)}.tabs a{color:#aeb5c6;text-decoration:none;padding:9px 15px;border-radius:10px;font-size:13px;font-weight:750;transition:.18s ease}.tabs a:hover{color:#fff;background:rgba(255,255,255,.05)}.tabs a[aria-current="page"]{background:linear-gradient(135deg,var(--accent),#b9d3ff);color:#0a1020;box-shadow:0 7px 20px rgba(67,116,190,.24)}
.card{position:relative;overflow:hidden;margin:15px 0;padding:22px;border:1px solid var(--line);border-radius:20px;background:linear-gradient(145deg,rgba(26,32,46,.91),rgba(14,18,27,.92));box-shadow:var(--shadow);backdrop-filter:blur(22px)}.card:after{content:"";position:absolute;inset:0;pointer-events:none;background:linear-gradient(115deg,rgba(255,255,255,.035),transparent 34%)}.card>*{position:relative;z-index:1}.grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:15px}.section-heading,.playing-head,.playlist-launch-head{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-bottom:19px}
label{display:grid;gap:7px;color:#cbd0dc;font-size:12px;font-weight:650}input,select{width:100%;border:1px solid var(--line);border-radius:12px;background:rgba(5,8,14,.72);color:#fff;padding:11px 13px;font:inherit;transition:border-color .18s,box-shadow .18s}input:focus,select:focus{outline:0;border-color:rgba(138,180,248,.75);box-shadow:0 0 0 4px rgba(138,180,248,.12)}input[type="range"]{accent-color:var(--accent);padding:8px 0}.check{display:flex;align-items:center;gap:9px;padding-top:24px}.check input{width:auto;accent-color:var(--accent)}
button{border:0;border-radius:12px;background:linear-gradient(135deg,var(--accent),#bad4ff);color:#07101e;font:inherit;font-weight:800;padding:11px 18px;cursor:pointer;margin-top:18px;box-shadow:0 8px 22px rgba(78,125,194,.18);transition:transform .16s ease,filter .16s ease,opacity .16s}button:hover:not(:disabled){transform:translateY(-1px);filter:brightness(1.07)}button:active:not(:disabled){transform:translateY(0)}button:disabled{cursor:not-allowed;opacity:.42}.button-subtle{background:rgba(255,255,255,.07);box-shadow:none;color:#d8deec}.button-danger{background:rgba(255,103,126,.11);box-shadow:none;color:#ffabb7}
.status{display:inline-flex;align-items:center;padding:6px 10px;border:1px solid rgba(115,226,167,.22);border-radius:999px;background:rgba(115,226,167,.08);color:var(--good);font-size:11px;font-weight:750}.status:before{content:"";width:7px;height:7px;border-radius:50%;background:currentColor;margin-right:7px;box-shadow:0 0 10px currentColor}.status.offline{color:var(--muted);border-color:var(--line);background:rgba(255,255,255,.03)}[hidden]{display:none!important}
.now-playing-body{display:flex;align-items:center;gap:22px}.now-playing-details{flex:1;min-width:0}.album-art-wrap{width:148px;aspect-ratio:1;flex:0 0 148px;border-radius:18px;overflow:hidden;background:#0a0d14;border:1px solid var(--line);box-shadow:0 18px 45px rgba(0,0,0,.4)}.album-art{display:block;width:100%;height:100%;object-fit:cover}.track-title{font-size:27px;line-height:1.15;font-weight:820;letter-spacing:-.035em;margin:3px 0 7px}.track-meta{color:var(--muted)}.web-progress{height:8px;background:rgba(255,255,255,.09);border-radius:99px;overflow:hidden;margin-top:22px;cursor:pointer}.web-progress:focus-visible{outline:2px solid var(--accent);outline-offset:4px}.web-progress-fill{height:100%;background:linear-gradient(90deg,var(--accent),var(--accent-2));width:0;transition:width .25s linear;pointer-events:none}.playback-time{display:flex;justify-content:space-between;color:#747d91;font-size:11px;margin-top:7px}.next-track{color:#bbc2d2;font-size:12px;margin-top:12px}.controls{display:grid;grid-template-columns:repeat(3,1fr);gap:9px;margin-top:19px}.controls button{margin:0}.controls button:not(:nth-child(2)){background:rgba(255,255,255,.065);box-shadow:none;color:#dbe1ed}.volume-row{display:grid;grid-template-columns:1fr auto;align-items:end;gap:10px;margin-top:17px}.volume-row button{margin:0}.js .volume-row button{display:none}.display-tools{display:flex;align-items:center;gap:12px;margin-top:15px}.display-tools button{margin:0;background:rgba(138,180,248,.1);box-shadow:none;color:#cfe0ff}.display-tools span{color:var(--muted);font-size:11px}.command-feedback{min-height:18px;color:var(--good);font-size:12px;margin-top:10px}.command-feedback.error{color:var(--danger)}.shortcuts{font-size:10px;margin-top:2px}
.notice{padding:12px 15px;border-radius:12px;background:rgba(115,226,167,.1);color:#aaf0c8;border:1px solid rgba(115,226,167,.22)}.playlist{border-top:1px solid var(--line);padding:17px 0 4px}.playlist:first-of-type{border-top:0;padding-top:0}.playlist-head{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-bottom:10px}.playlist-launch,.library-search-form{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:12px;align-items:end}.playlist-launch label+label{margin-top:12px}.playlist-launch button,.library-search-form button{margin:0;height:46px}.playlist-select{min-height:178px}.playlist-count{font-size:11px;color:var(--muted)}
.search-results{display:grid;gap:8px;margin-top:14px}.search-result,.history-item{display:flex;align-items:center;justify-content:space-between;gap:14px}.search-result,.queue-item,.history-item{padding:13px;border:1px solid var(--line);border-radius:14px;background:rgba(5,8,14,.46)}.queue-item{display:grid;grid-template-columns:minmax(0,1fr) auto;align-items:center;gap:14px}.queue-item.current{border-color:rgba(138,180,248,.5);background:rgba(138,180,248,.08)}.search-result-copy{display:grid;gap:4px;min-width:0}.queue-item-copy{width:100%;overflow:hidden}.search-result-copy strong,.search-result-copy span{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.search-result-copy span{color:var(--muted);font-size:11px}.search-actions,.queue-actions,.queue-controls{display:flex;align-items:center;gap:6px;flex:0 0 auto}.search-actions button,.queue-actions button,.browse-item{margin:0;padding:8px 10px;background:rgba(255,255,255,.07);box-shadow:none;color:#cfe0ff;font-size:11px}.search-actions button:first-child{background:linear-gradient(135deg,var(--accent),#bad4ff);color:#07101e}.browse-toolbar{display:grid;grid-template-columns:160px 1fr;gap:10px}.browse-items{display:flex;gap:7px;overflow:auto;padding:10px 0 3px}.browse-item{white-space:nowrap}.queue-heading{display:flex;align-items:center;justify-content:space-between;gap:12px}.queue-heading button{margin:0}.queue-list,.history-list{display:grid;gap:8px;margin-top:14px}.diagnostic-grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:10px}.diagnostic{padding:14px;border:1px solid var(--line);border-radius:14px;background:rgba(5,8,14,.46)}.diagnostic span{display:block;color:var(--muted);font-size:11px}.diagnostic strong{display:block;margin-top:4px;font-size:17px}.hint{font-size:11px}.install-actions,.settings-actions{display:flex;align-items:center;gap:12px;flex-wrap:wrap}.install-actions button,.settings-actions button{margin:10px 0 0}.install-status{flex:1;min-width:220px}.security-note{display:flex;gap:12px;align-items:flex-start;padding:13px;border:1px solid rgba(138,180,248,.15);border-radius:13px;background:rgba(138,180,248,.055)}.security-note strong{display:block;margin-bottom:2px}.login-shell{width:min(430px,calc(100% - 32px));padding-top:9vh}.login-card{padding:28px}.login-card h2{font-size:25px;margin-bottom:6px}.login-card button{width:100%}.login-error{color:var(--danger);margin:12px 0}.footer{font-size:11px;text-align:center;margin-top:26px}.footer a{color:#aebddd}
.queue-vote{flex:0 0 88px;width:88px;white-space:nowrap;margin:0;padding:8px 10px;box-shadow:none;color:var(--good);background:rgba(115,226,167,.08);font-size:11px}.client-identity{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-bottom:14px;padding:10px 12px;border:1px solid var(--line);border-radius:12px;background:rgba(5,8,14,.42)}.client-identity span{display:block;color:var(--muted);font-size:10px;text-transform:uppercase;letter-spacing:.08em}.client-identity strong{display:block;margin-top:1px}.client-identity button{margin:0;padding:7px 10px;background:rgba(255,255,255,.07);box-shadow:none;color:#d8deec;font-size:11px}
@media(display-mode:standalone){main{padding-top:max(24px,env(safe-area-inset-top));padding-bottom:max(32px,env(safe-area-inset-bottom))}}
@media(max-width:700px){main{width:min(100% - 22px,1040px);padding-top:18px}.app-header{align-items:flex-start;flex-direction:column}.tabs{width:100%;overflow:auto}.tabs a{flex:1;text-align:center;white-space:nowrap}.grid,.diagnostic-grid{grid-template-columns:1fr}.check{padding-top:0}.volume-row,.playlist-launch,.library-search-form,.browse-toolbar{grid-template-columns:1fr}.search-result,.history-item{align-items:stretch;flex-direction:column}.queue-item{grid-template-columns:1fr;align-items:stretch}.search-actions{display:grid;grid-template-columns:repeat(3,1fr)}.queue-controls{display:grid;grid-template-columns:1fr;width:100%}.queue-actions{display:grid;grid-template-columns:repeat(4,1fr)}.queue-vote{width:100%}.now-playing-body{gap:14px}.album-art-wrap{width:100px;flex-basis:100px}.track-title{font-size:21px}.card{padding:17px;border-radius:17px}.brand-mark{width:40px;height:40px}.shortcuts{display:none}}
"#;

fn page_start(active_page: &str) -> String {
    let mut html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1,viewport-fit=cover\">\
         <meta name=\"theme-color\" content=\"#080b12\"><meta name=\"application-name\" content=\"Orpheus\">\
         <meta name=\"apple-mobile-web-app-capable\" content=\"yes\"><meta name=\"apple-mobile-web-app-status-bar-style\" content=\"black-translucent\"><meta name=\"apple-mobile-web-app-title\" content=\"Orpheus\">\
         <link rel=\"manifest\" href=\"/manifest.webmanifest\"><link rel=\"icon\" href=\"/icon-192.png\" sizes=\"192x192\" type=\"image/png\"><link rel=\"apple-touch-icon\" href=\"/icon-192.png\"><script src=\"/identity.js?v=1\" defer></script>\
         <title>Orpheus</title><style>{WEB_STYLE}</style></head><body class=\"identity-pending\">{IDENTITY_GATE_HTML}<main inert{}>\
         <header class=\"app-header\"><div class=\"brand\"><div class=\"brand-mark\">O</div><div><div class=\"eyebrow\">Now playing</div><h1>Orpheus</h1><p>Music, beautifully simple.</p></div></div>",
        if active_page == "login" {
            " class=\"login-shell\""
        } else {
            ""
        }
    );
    if active_page != "login" {
        let _ = write!(
            html,
            "<nav class=\"tabs\" aria-label=\"Application sections\"><a href=\"/\"{}>Playback</a><a href=\"/history\"{}>History</a><a href=\"/diagnostics\"{}>Diagnostics</a><a href=\"/settings\"{}>Configuration</a></nav>",
            if active_page == "player" {
                " aria-current=\"page\""
            } else {
                ""
            },
            if active_page == "history" {
                " aria-current=\"page\""
            } else {
                ""
            },
            if active_page == "diagnostics" {
                " aria-current=\"page\""
            } else {
                ""
            },
            if active_page == "settings" {
                " aria-current=\"page\""
            } else {
                ""
            },
        );
    } else {
        html.push_str("<a href=\"/\" class=\"button-subtle\">Guest player</a>");
    }
    html.push_str("</header>");
    html
}

fn render_player_page(
    playback: &WebPlaybackStatus,
    playlists: &[WebPlaylist],
    csrf_token: &str,
    notice: Option<&str>,
    guest_restricted: bool,
    guest_cooldown_seconds: u64,
) -> String {
    let mut html = page_start("player");
    if guest_restricted {
        let _ = write!(
            html,
            "<input id=\"guest-mode\" type=\"hidden\"><div class=\"notice\">Guest mode: play/pause and volume are available. You may add one song every {} seconds; other controls require the settings login.</div>",
            guest_cooldown_seconds
        );
    }
    if let Some(notice) = notice {
        let _ = write!(html, "<div class=\"notice\">{}</div>", escape_html(notice));
    }
    html.push_str(&playback_card(playback, csrf_token, !guest_restricted));
    html.push_str(&library_search_card(playback.mopidy_online, csrf_token));
    html.push_str(&library_browser_card());
    html.push_str(&queue_editor_card(!guest_restricted));
    if !guest_restricted {
        html.push_str(&playlist_launcher(
            playlists,
            playback.mopidy_online,
            csrf_token,
        ));
    }
    html.push_str("<p class=\"footer\">Local multi-client player · <a href=\"/health\">health</a></p></main><script src=\"/app.js?v=9\" defer></script><script src=\"/pwa.js\" defer></script></body></html>");
    html
}

fn library_search_card(online: bool, csrf_token: &str) -> String {
    let disabled = if online { "" } else { " disabled" };
    format!(
        "<section class=\"card\"><h2>Find songs</h2>\
         <div class=\"client-identity\"><div><span>Queue identity</span><strong id=\"client-name-label\">Not set</strong></div><button id=\"change-client-name\" type=\"button\">Change</button></div><input id=\"requester-name\" type=\"hidden\">\
         <form id=\"library-search-form\" class=\"library-search-form\" method=\"get\" action=\"/api/search\">\
         <label>Search Jellyfin<input id=\"library-search-input\" type=\"search\" name=\"q\" minlength=\"3\" maxlength=\"120\" placeholder=\"Song, artist, or album…\" autocomplete=\"off\"></label>\
         <button id=\"library-search-button\" data-mopidy-control type=\"submit\"{}>Search</button></form>\
         <input id=\"library-search-csrf\" type=\"hidden\" value=\"{}\">\
         <p id=\"library-search-status\" class=\"hint\" aria-live=\"polite\">Search your Jellyfin music library.</p>\
         <div id=\"library-search-results\" class=\"search-results\"></div></section>",
        disabled,
        escape_html(csrf_token)
    )
}

fn library_browser_card() -> String {
    "<section class=\"card\"><h2>Browse library</h2><div class=\"browse-toolbar\"><label>Browse by<select id=\"library-browse-kind\"><option value=\"artists\">Artist</option><option value=\"albums\">Album</option></select></label><label>Filter<input id=\"library-browse-filter\" type=\"search\" placeholder=\"Filter this list…\" autocomplete=\"off\"></label></div><p id=\"library-browse-status\" class=\"hint\" aria-live=\"polite\">Loading artists…</p><div id=\"library-browse-items\" class=\"browse-items\"></div><div id=\"library-browse-tracks\" class=\"search-results\"></div></section>".to_string()
}

fn queue_editor_card(can_edit: bool) -> String {
    format!(
        "<section class=\"card\"><div class=\"queue-heading\"><h2>Mopidy queue</h2>{}</div><p id=\"queue-status\" class=\"hint\">Loading queue…</p><div id=\"queue-list\" class=\"queue-list\"></div></section>",
        if can_edit {
            "<button id=\"queue-clear\" class=\"button-danger\" type=\"button\">Clear queue</button>"
        } else {
            ""
        }
    )
}

fn render_history_page(history: &[HistoryEntry], _csrf_token: &str) -> String {
    let mut html = page_start("history");
    html.push_str("<section class=\"card\"><div class=\"section-heading\"><div><div class=\"eyebrow\">Listening log</div><h2>Playback history</h2></div></div><div class=\"history-list\">");
    if history.is_empty() {
        html.push_str("<p class=\"hint\">No tracks have been recorded yet.</p>");
    }
    for entry in history {
        let _ = write!(
            html,
            "<article class=\"history-item\"><div class=\"search-result-copy\"><strong>{}</strong><span>{} · {} · {}</span></div><span class=\"hint\">{}</span></article>",
            escape_html(&entry.title),
            escape_html(&entry.artist),
            escape_html(&entry.album),
            escape_html(&entry.source),
            escape_html(&format_unix_time(entry.played_at_unix)),
        );
    }
    html.push_str("</div><p class=\"hint\">The newest 250 tracks are kept locally on the Pi.</p></section></main></body></html>");
    html
}

fn render_diagnostics_page(diagnostics: &WebDiagnostics) -> String {
    let mut html = page_start("diagnostics");
    let temperature = diagnostics
        .cpu_temperature_c
        .map(|value| format!("{value:.1} °C"))
        .unwrap_or_else(|| "Unavailable".to_string());
    let values = [
        ("Active source", diagnostics.source.clone()),
        (
            "Playback source",
            online_label(diagnostics.playback_online).to_string(),
        ),
        (
            "Mopidy",
            online_label(diagnostics.mopidy_online).to_string(),
        ),
        (
            "Spotifyd",
            online_label(diagnostics.spotifyd_available).to_string(),
        ),
        ("Now-playing view", diagnostics.now_playing_style.clone()),
        ("CPU temperature", temperature),
        (
            "App uptime",
            format_duration(diagnostics.app_uptime_seconds),
        ),
        (
            "System uptime",
            format_duration(diagnostics.system_uptime_seconds),
        ),
        ("Queue", format!("{} tracks", diagnostics.queue_tracks)),
        (
            "History",
            format!("{} entries", diagnostics.history_entries),
        ),
        (
            "Artwork cache",
            format!(
                "{} files · {}",
                diagnostics.artwork_cache_files,
                format_bytes(diagnostics.artwork_cache_bytes)
            ),
        ),
        (
            "Persistent queue",
            if diagnostics.persistent_queue_file {
                "Saved".to_string()
            } else {
                "Not written yet".to_string()
            },
        ),
    ];
    html.push_str("<section class=\"card\"><div class=\"section-heading\"><div><div class=\"eyebrow\">Private status</div><h2>Diagnostics</h2></div></div><div class=\"diagnostic-grid\">");
    for (label, value) in values {
        let _ = write!(
            html,
            "<div class=\"diagnostic\"><span>{}</span><strong>{}</strong></div>",
            escape_html(label),
            escape_html(&value)
        );
    }
    html.push_str("</div><p class=\"hint\">Refresh this page for current values. No credentials or private configuration values are displayed.</p></section></main></body></html>");
    html
}

fn online_label(value: bool) -> &'static str {
    if value { "Online" } else { "Offline" }
}

fn format_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else {
        format!("{hours}h {minutes}m")
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{:.1} KiB", bytes as f64 / 1_024.0)
    }
}

fn format_unix_time(seconds: u64) -> String {
    let timestamp = seconds as libc::time_t;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    let result = unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()) };
    if result.is_null() {
        return seconds.to_string();
    }
    let local = unsafe { local.assume_init() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min
    )
}

fn render_login_page(csrf_token: &str, error: Option<&str>, notice: Option<&str>) -> String {
    let mut html = page_start("login");
    html.push_str("<section class=\"card login-card\"><div class=\"eyebrow\">Private controls</div><h2>Welcome back</h2><p>Guests can play, pause, and change volume. Sign in for playlist controls, queue editing, history, diagnostics, and configuration.</p>");
    if let Some(notice) = notice {
        let _ = write!(html, "<div class=\"notice\">{}</div>", escape_html(notice));
    }
    if let Some(error) = error {
        let _ = write!(html, "<p class=\"login-error\">{}</p>", escape_html(error));
    }
    let _ = write!(
        html,
        "<form method=\"post\" action=\"/login\"><input type=\"hidden\" name=\"csrf\" value=\"{}\">\
         <label>Settings password<input type=\"password\" name=\"password\" autocomplete=\"current-password\" autofocus required></label>\
         <button type=\"submit\">Unlock configuration</button></form></section>\
         <p class=\"footer\">Private session expires after 12 hours.</p></main></body></html>",
        escape_html(csrf_token)
    );
    html
}

fn render_settings_page(
    settings: &Settings,
    config: &Config,
    csrf_token: &str,
    notice: Option<&str>,
) -> String {
    let idle_mode = if !settings.screensaver_enabled {
        "dim"
    } else if settings.screensaver_heavy_dim {
        "screensaver_dim"
    } else {
        "screensaver"
    };
    let mut html = String::from(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1,viewport-fit=cover\">\
         <meta name=\"theme-color\" content=\"#0d0e10\"><meta name=\"application-name\" content=\"Orpheus\"><meta name=\"apple-mobile-web-app-capable\" content=\"yes\"><meta name=\"apple-mobile-web-app-status-bar-style\" content=\"black-translucent\"><meta name=\"apple-mobile-web-app-title\" content=\"Orpheus\">\
         <link rel=\"manifest\" href=\"/manifest.webmanifest\"><link rel=\"icon\" href=\"/icon-192.png\" sizes=\"192x192\" type=\"image/png\"><link rel=\"apple-touch-icon\" href=\"/icon-192.png\">\
         <title>Orpheus</title><style>\
         :root{color-scheme:dark;--bg:#0d0e10;--card:#191a1d;--line:#303136;--muted:#96979d;--accent:#8ab4f8}\
         *{box-sizing:border-box}body{margin:0;background:var(--bg);color:#f5f5f5;font:15px system-ui,sans-serif;padding-left:env(safe-area-inset-left);padding-right:env(safe-area-inset-right)}\
         main{width:min(920px,calc(100% - 28px));margin:32px auto 64px}header{margin-bottom:16px}\
         h1{font-size:28px;margin:0 0 6px}h2{font-size:19px;margin:0 0 18px}p{color:var(--muted);margin:5px 0}a{color:var(--accent)}.tabs{display:flex;gap:8px;margin:0 0 22px}.tabs a{color:#c5c6ca;text-decoration:none;padding:9px 14px;border:1px solid var(--line);border-radius:9px;font-weight:700}.tabs a[aria-current=\"page\"]{background:var(--accent);border-color:var(--accent);color:#101114}\
         .grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:14px}.card{background:var(--card);border:1px solid var(--line);border-radius:14px;padding:20px;margin:16px 0}\
         label{display:grid;gap:7px;color:#d7d7da;font-size:13px}input,select{width:100%;border:1px solid #414248;border-radius:8px;background:#101114;color:#fff;padding:10px 11px;font:inherit}\
         input:focus,select:focus{outline:2px solid var(--accent);border-color:transparent}.check{display:flex;align-items:center;gap:9px;padding-top:24px}.check input{width:auto}\
         button{border:0;border-radius:9px;background:var(--accent);color:#101114;font-weight:700;padding:11px 18px;cursor:pointer;margin-top:18px}button:disabled{cursor:not-allowed;opacity:.45}\
         .playing-head{display:flex;align-items:center;justify-content:space-between;gap:12px}.status{font-size:12px;color:var(--muted)}.status::before{content:'';display:inline-block;width:8px;height:8px;border-radius:50%;background:#6fdc8c;margin-right:7px}.status.offline::before{background:#777}\
         [hidden]{display:none!important}.now-playing-body{display:flex;align-items:flex-start;gap:18px}.now-playing-details{flex:1;min-width:0}.album-art-wrap{width:132px;aspect-ratio:1;flex:0 0 132px;border-radius:12px;overflow:hidden;background:#101114;border:1px solid var(--line)}.album-art{display:block;width:100%;height:100%;object-fit:cover}.track-title{font-size:22px;font-weight:750;margin:4px 0}.track-meta{color:var(--muted)}.web-progress{height:9px;background:#303136;border-radius:6px;overflow:hidden;margin-top:16px;cursor:pointer}.web-progress:focus-visible{outline:2px solid var(--accent);outline-offset:3px}.web-progress-fill{height:100%;background:var(--accent);width:0;transition:width .25s linear;pointer-events:none}.playback-time{display:flex;justify-content:space-between;color:var(--muted);font-size:12px;margin-top:6px}.next-track{color:#c5c6ca;font-size:13px;margin-top:12px}.controls{display:grid;grid-template-columns:repeat(3,1fr);gap:10px;margin-top:18px}.controls button{margin:0}.volume-row{display:grid;grid-template-columns:1fr auto;align-items:end;gap:10px;margin-top:16px}.volume-row button{margin:0}.js .volume-row button{display:none}.display-tools{display:flex;align-items:center;gap:12px;margin-top:14px}.display-tools button{margin:0;background:#303c50;color:#cfe0ff}.display-tools span{color:var(--muted);font-size:12px}.command-feedback{min-height:18px;color:#9cd7aa;font-size:12px;margin-top:10px}.command-feedback.error{color:#ff9c9c}.shortcuts{font-size:11px;margin-top:2px}\
         .notice{padding:12px 15px;border-radius:9px;background:#17351f;color:#a8e6b5;border:1px solid #285d35}.playlist{border-top:1px solid var(--line);padding:16px 0 4px}.playlist:first-of-type{border-top:0;padding-top:0}\
         .playlist-head{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-bottom:10px}.playlist-launch-head{display:flex;align-items:center;justify-content:space-between;gap:12px}.playlist-launch{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:12px;align-items:end}.playlist-launch label+label{margin-top:12px}.playlist-launch button{margin:0;height:43px}.playlist-select{min-height:178px}.playlist-count{font-size:12px;color:var(--muted)}.hint{font-size:12px}.install-actions{display:flex;align-items:center;gap:14px;flex-wrap:wrap}.install-actions button{margin:10px 0 0}.install-status{flex:1;min-width:220px}.footer{font-size:12px;text-align:center;margin-top:24px}@media(display-mode:standalone){main{margin-top:max(18px,env(safe-area-inset-top));margin-bottom:max(28px,env(safe-area-inset-bottom))}}@media(max-width:650px){.grid{grid-template-columns:1fr}.check{padding-top:0}.volume-row,.playlist-launch{grid-template-columns:1fr}.now-playing-body{gap:12px}.album-art-wrap{width:92px;flex-basis:92px}main{margin-top:20px}}\
         </style></head><body><main><header><h1>Orpheus</h1><p>A lightweight music player and display controller for your local network.</p></header><nav class=\"tabs\" aria-label=\"Application sections\"><a href=\"/\">Playback</a><a href=\"/settings\" aria-current=\"page\">Configuration</a></nav>",
    );
    html = html.replacen(
        "</head>",
        &format!(
            "<style>{WEB_STYLE}</style><script src=\"/identity.js?v=1\" defer></script></head>"
        ),
        1,
    );
    html = html.replacen(
        "<body><main>",
        &format!("<body class=\"identity-pending\">{IDENTITY_GATE_HTML}<main inert>"),
        1,
    );
    html = html.replacen(
        "<header><h1>Orpheus</h1><p>A lightweight music player and display controller for your local network.</p></header><nav class=\"tabs\" aria-label=\"Application sections\"><a href=\"/\">Playback</a><a href=\"/settings\" aria-current=\"page\">Configuration</a></nav>",
        "<header class=\"app-header\"><div class=\"brand\"><div class=\"brand-mark\">O</div><div><div class=\"eyebrow\">Private controls</div><h1>Orpheus</h1><p>Device configuration.</p></div></div><nav class=\"tabs\" aria-label=\"Application sections\"><a href=\"/\">Playback</a><a href=\"/history\">History</a><a href=\"/diagnostics\">Diagnostics</a><a href=\"/settings\" aria-current=\"page\">Configuration</a></nav></header>",
        1,
    );
    if let Some(notice) = notice {
        let _ = write!(html, "<div class=\"notice\">{}</div>", escape_html(notice));
    }
    html.push_str(
        "<div id=\"settings-feedback\" class=\"command-feedback\" aria-live=\"polite\"></div>",
    );

    let _ = write!(
        html,
        "<form id=\"settings-form\" method=\"post\" action=\"/settings\"><input type=\"hidden\" name=\"csrf\" value=\"{}\">\
         <section class=\"card\"><h2>Display &amp; idle</h2><div class=\"grid\">\
         {}{}{}{}{}{}{}{}{}\
         </div></section>",
        escape_html(csrf_token),
        number_field(
            "Display brightness (%)",
            "display_brightness",
            settings.display_brightness * 100.0,
            10.0,
            100.0,
            "1"
        ),
        number_field(
            "Dim timeout (seconds, 0 = off)",
            "dim_timeout_seconds",
            settings.dim_timeout_seconds as f64,
            0.0,
            86400.0,
            "1"
        ),
        number_field(
            "Heavy dim brightness (%)",
            "dim_brightness",
            settings.dim_brightness * 100.0,
            0.0,
            100.0,
            "1"
        ),
        select_field(
            "Idle display",
            "idle_mode",
            idle_mode,
            &[
                ("dim", "Dim only"),
                ("screensaver", "Screensaver"),
                ("screensaver_dim", "Screensaver + heavy dim")
            ]
        ),
        select_field(
            "Screensaver style",
            "screensaver_style",
            match settings.screensaver_style {
                ScreensaverStyle::Albums => "albums",
                ScreensaverStyle::Clock => "clock",
            },
            &[("albums", "Album carousel"), ("clock", "Clock + weather")]
        ),
        number_field(
            "Screensaver brightness (%)",
            "screensaver_brightness",
            settings.screensaver_brightness * 100.0,
            5.0,
            100.0,
            "1"
        ),
        checkbox(
            "Shuffle album screensaver",
            "shuffle_screensaver_art",
            settings.shuffle_screensaver_art
        ),
        carousel_speed_field(settings.carousel_speed),
        checkbox(
            "Shuffle playlists when starting",
            "shuffle_playlists",
            settings.shuffle_playlists
        ),
    );
    let _ = write!(
        html,
        "<section class=\"card\"><div class=\"section-heading\"><div><div class=\"eyebrow\">Listening</div><h2>Playback &amp; weather</h2></div></div><div class=\"grid\">{}{}{}{}{}{}{}{}{}\
         <div><p class=\"hint\">Leave the player maximum at 100% for smooth Spotify-app volume control. The hardware ceiling is the instantaneous HiFiBerry safety limit; a lower software maximum can only correct a Spotify request after it arrives. Weather coordinates are sent to Open-Meteo every 15 minutes.</p></div></div></section>",
        number_field(
            "Volume step (%)",
            "volume_step",
            settings.volume_step as f64,
            1.0,
            25.0,
            "1"
        ),
        number_field(
            "Maximum player volume (%)",
            "max_volume",
            settings.max_volume as f64,
            10.0,
            100.0,
            "1"
        ),
        number_field(
            "Hardware output ceiling (dB)",
            "hardware_volume_ceiling_db",
            settings.hardware_volume_ceiling_db,
            -40.0,
            0.0,
            "0.5"
        ),
        select_field(
            "Now-playing display",
            "now_playing_style",
            match settings.now_playing_style {
                NowPlayingStyle::Artwork => "artwork",
                NowPlayingStyle::Visualizer => "visualizer",
                NowPlayingStyle::TrackInfo => "track_info",
            },
            &[
                ("artwork", "Album artwork"),
                ("visualizer", "Audio visualizer"),
                ("track_info", "Track information")
            ]
        ),
        visualizer_delay_field(settings.visualizer_delay_ms),
        spotify_visualizer_delay_field(settings.spotify_visualizer_extra_delay_ms),
        text_field(
            "Weather label",
            "weather_location",
            &settings.weather_location
        ),
        number_field(
            "Latitude",
            "weather_latitude",
            settings.weather_latitude,
            -90.0,
            90.0,
            "0.00001"
        ),
        number_field(
            "Longitude",
            "weather_longitude",
            settings.weather_longitude,
            -180.0,
            180.0,
            "0.00001"
        ),
    );

    let _ = write!(
        html,
        "<section class=\"card\"><div class=\"section-heading\"><div><div class=\"eyebrow\">Daily dashboard</div><h2>Good morning mode</h2></div></div>\
         <div class=\"grid\">{}{}{}{}\
         <div><p class=\"hint\">Headlines are read as a private RSS feed every 20 minutes. Clear the URL to disable news while keeping weather and the daily quote.</p></div></div></section>\
         <section class=\"card\"><div class=\"section-heading\"><div><div class=\"eyebrow\">Private</div><h2>Household &amp; guest access</h2></div></div>\
         <div class=\"security-note\"><div>🔒</div><div><strong>Guest mode permits play/pause, volume, voting, and rate-limited song requests.</strong><p>Track navigation, seeking, queue editing, playlist launching, history, diagnostics, and configuration remain private.</p></div></div>\
         <div class=\"grid\">{}{}</div>\
         <label>New settings password<input type=\"password\" name=\"new_web_settings_password\" minlength=\"10\" maxlength=\"128\" autocomplete=\"new-password\" placeholder=\"Keep current password\"></label>\
         <div class=\"settings-actions\"><button type=\"submit\">Save all settings</button></div></section></form>\
         <form method=\"post\" action=\"/logout\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><button class=\"button-danger\" type=\"submit\">Sign out of configuration</button></form>",
        checkbox(
            "Enable the morning dashboard",
            "morning_mode_enabled",
            settings.morning_mode_enabled
        ),
        number_field(
            "Start hour (local time)",
            "morning_start_hour",
            settings.morning_start_hour as f64,
            0.0,
            23.0,
            "1"
        ),
        number_field(
            "End hour (local time)",
            "morning_end_hour",
            settings.morning_end_hour as f64,
            1.0,
            24.0,
            "1"
        ),
        text_field(
            "News RSS feed",
            "morning_news_feed_url",
            &settings.morning_news_feed_url
        ),
        checkbox(
            "Enable rate-limited guest mode",
            "guest_mode_enabled",
            settings.guest_mode_enabled
        ),
        number_field(
            "Guest queue cooldown (seconds)",
            "guest_queue_cooldown_seconds",
            settings.guest_queue_cooldown_seconds as f64,
            30.0,
            3_600.0,
            "1"
        ),
        escape_html(csrf_token),
    );

    let _ = write!(
        html,
        "<form method=\"post\" action=\"/playlists\"><input type=\"hidden\" name=\"csrf\" value=\"{}\">\
         <section class=\"card\"><h2>Pinned favourites</h2><p class=\"hint\">These appear first in the web player and on the physical display. Clear all fields in a row to remove it.</p>",
        escape_html(csrf_token)
    );
    let row_count = config.playlists.len() + 3;
    for index in 0..row_count {
        let entry = config.playlists.get(index);
        let _ = write!(
            html,
            "<div class=\"playlist\"><div class=\"playlist-head\"><strong>Playlist {}</strong></div><div class=\"grid\">{}{}{}</div></div>",
            index + 1,
            text_field(
                "Name",
                &format!("playlist_name_{index}"),
                entry.map(|entry| entry.name.as_str()).unwrap_or_default()
            ),
            text_field(
                "Mopidy URI",
                &format!("playlist_uri_{index}"),
                entry.map(|entry| entry.uri.as_str()).unwrap_or_default()
            ),
            text_field(
                "Artwork URI (optional)",
                &format!("playlist_art_{index}"),
                entry
                    .and_then(|entry| entry.art_uri.as_deref())
                    .unwrap_or_default()
            ),
        );
    }
    html.push_str(
        "<button type=\"submit\">Save favourites</button></section></form>\
         <section class=\"card\"><h2>Install application</h2><p>Install Orpheus for a standalone window, home-screen icon, and quick access to playback and configuration.</p>\
         <div class=\"install-actions\"><button id=\"install-app\" type=\"button\">Install Orpheus</button><p id=\"install-status\" class=\"install-status hint\">Checking browser installation support…</p></div>\
         <p class=\"hint\">Installation stays local and does not add a cloud service. Browser support varies; local HTTP addresses may offer Add to Home screen instead of a direct prompt.</p></section>\
         <p class=\"footer\">Local multi-client player · <a href=\"/health\">health</a></p></main><script src=\"/settings.js\" defer></script><script src=\"/pwa.js\" defer></script></body></html>",
    );
    html
}

fn playlist_launcher(playlists: &[WebPlaylist], online: bool, csrf_token: &str) -> String {
    let mut options = String::new();
    if playlists.is_empty() {
        options.push_str("<option value=\"\" disabled selected>Loading playlists…</option>");
    } else {
        for (index, playlist) in playlists.iter().enumerate() {
            let label = if playlist.favorite {
                format!("★ {}", playlist.name)
            } else {
                playlist.name.clone()
            };
            let _ = write!(
                options,
                "<option value=\"{}\" data-name=\"{}\"{}>{}</option>",
                escape_html(&playlist.uri),
                escape_html(&playlist.name),
                if index == 0 { " selected" } else { "" },
                escape_html(&label)
            );
        }
    }

    let disabled = if online && !playlists.is_empty() {
        ""
    } else {
        " disabled"
    };
    format!(
        "<section class=\"card\"><div class=\"playlist-launch-head\"><h2>Play a playlist</h2><span id=\"playlist-count\" class=\"playlist-count\">{} available</span></div>\
         <form id=\"quick-playlist-form\" class=\"playlist-launch\" method=\"post\" action=\"/library/play\"><input id=\"quick-playlist-csrf\" type=\"hidden\" name=\"csrf\" value=\"{}\">\
         <div><label>Search<input id=\"playlist-filter\" type=\"search\" placeholder=\"Type a playlist name…\" autocomplete=\"off\"></label>\
         <label>Playlist<select id=\"playlist-select\" class=\"playlist-select\" name=\"playlist_uri\" size=\"7\" required>{}</select></label></div>\
         <button id=\"quick-playlist-play\" data-mopidy-control type=\"submit\"{}>Play selected</button></form>\
         <p class=\"hint\">Choosing a playlist replaces the queue. ★ favourites stay at the top; the full Mopidy catalog refreshes automatically.</p></section>",
        playlists.len(),
        escape_html(csrf_token),
        options,
        disabled
    )
}

fn playback_card(playback: &WebPlaybackStatus, csrf_token: &str, controls_allowed: bool) -> String {
    let status_text = if !playback.online {
        format!("{} unavailable", playback.source)
    } else if playback.is_playing {
        format!("Playing · {}", playback.source)
    } else {
        format!("Paused / stopped · {}", playback.source)
    };
    let status_class = if playback.online {
        "status"
    } else {
        "status offline"
    };
    let toggle_label = if playback.is_playing { "Pause" } else { "Play" };
    let screen_toggle_label = if playback.screensaver_active {
        "Wake display"
    } else {
        "Preview screensaver"
    };
    let cover_label = format!(
        "{} album cover{} ready · {:.0} px/s",
        playback.carousel_cover_count,
        if playback.carousel_cover_count == 1 {
            ""
        } else {
            "s"
        },
        playback.carousel_speed
    );
    let volume = playback.volume.unwrap_or(50);
    let privileged_disabled = if playback.online && controls_allowed {
        ""
    } else {
        " disabled"
    };
    let playback_disabled = if playback.online { "" } else { " disabled" };
    let screen_disabled = if controls_allowed { "" } else { " disabled" };
    let duration = playback.duration_seconds.max(0.0);
    let position = playback.position_seconds.max(0.0);
    let progress = if duration > 0.0 {
        (position / duration * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let remaining = if duration > 0.0 {
        format!("-{}", format_web_time((duration - position).max(0.0)))
    } else {
        "--:--".to_string()
    };
    let next_description = match (&playback.next_artist, &playback.next_title) {
        (Some(artist), Some(title)) if !artist.is_empty() => format!("{artist} — {title}"),
        (_, Some(title)) => title.clone(),
        _ => "queue end".to_string(),
    };
    let artwork_id = playback.artwork_id.as_deref().unwrap_or_default();
    let artwork_hidden = if playback.artwork_id.is_some() {
        ""
    } else {
        " hidden"
    };
    let artwork_src = playback
        .artwork_id
        .as_deref()
        .map(|id| format!(" src=\"/api/artwork?v={}\"", escape_html(id)))
        .unwrap_or_default();
    let artwork_alt = playback
        .artwork_id
        .as_ref()
        .map(|_| format!("Album artwork for {}", playback.title))
        .unwrap_or_default();

    format!(
        "<section class=\"card\"><div class=\"playing-head\"><h2>Now playing</h2><span id=\"playback-status\" class=\"{}\">{}</span></div>\
         <div class=\"now-playing-body\"><div id=\"album-art-wrap\" class=\"album-art-wrap\"{}><img id=\"album-art\" class=\"album-art\" data-artwork-id=\"{}\"{} alt=\"{}\"></div><div class=\"now-playing-details\">\
         <div id=\"track-title\" class=\"track-title\">{}</div><div id=\"track-meta\" class=\"track-meta\">{} · {}</div>\
         <div id=\"playback-progress-bar\" class=\"web-progress\" role=\"slider\" tabindex=\"0\" title=\"Click or use arrow keys to seek\" aria-label=\"Track position\" aria-valuemin=\"0\" aria-valuemax=\"100\" aria-valuenow=\"{:.0}\"><div id=\"playback-progress\" class=\"web-progress-fill\" style=\"width:{:.2}%\"></div></div>\
         <div class=\"playback-time\"><span id=\"elapsed-time\">{}</span><span id=\"remaining-time\">{}</span></div>\
         <div id=\"next-track\" class=\"next-track\">Next: {}</div></div></div>\
         <form id=\"playback-form\" method=\"post\" action=\"/playback\"><input id=\"playback-csrf\" type=\"hidden\" name=\"csrf\" value=\"{}\">\
         <div class=\"controls\"><button data-playback-control data-admin-control type=\"submit\" name=\"action\" value=\"previous\"{}>Previous</button>\
         <button id=\"toggle-playback\" data-playback-control type=\"submit\" name=\"action\" value=\"toggle\"{}>{}</button>\
         <button data-playback-control data-admin-control type=\"submit\" name=\"action\" value=\"next\"{}>Next</button></div>\
         <div class=\"volume-row\"><label><span id=\"volume-label\">Volume ({}%)</span><input id=\"playback-volume\" data-playback-control type=\"range\" name=\"volume\" value=\"{}\" min=\"0\" max=\"{}\" step=\"1\"{}></label>\
         <button data-playback-control type=\"submit\" name=\"action\" value=\"volume\"{}>Set volume</button></div>\
         <div class=\"display-tools\"><button id=\"toggle-screen\" data-admin-control type=\"submit\" name=\"action\" value=\"screensaver\"{}>{}</button><span id=\"carousel-status\">{}</span></div></form>\
         <div id=\"command-feedback\" class=\"command-feedback\" aria-live=\"polite\"></div><p class=\"shortcuts\">Shortcuts: Space play/pause · ←/→ seek 10s · ↑/↓ volume · N/P tracks · S screen</p></section>",
        status_class,
        escape_html(&status_text),
        artwork_hidden,
        escape_html(artwork_id),
        artwork_src,
        escape_html(&artwork_alt),
        escape_html(&playback.title),
        escape_html(&playback.artist),
        escape_html(&playback.album),
        progress,
        progress,
        format_web_time(position),
        remaining,
        escape_html(&next_description),
        escape_html(csrf_token),
        privileged_disabled,
        playback_disabled,
        toggle_label,
        privileged_disabled,
        volume,
        volume,
        playback.max_volume,
        playback_disabled,
        playback_disabled,
        screen_disabled,
        screen_toggle_label,
        escape_html(&cover_label),
    )
}

fn format_web_time(seconds: f64) -> String {
    let total = if seconds.is_finite() {
        seconds.max(0.0).floor() as u64
    } else {
        0
    };
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn text_field(label: &str, name: &str, value: &str) -> String {
    format!(
        "<label>{}<input name=\"{}\" value=\"{}\"></label>",
        escape_html(label),
        escape_html(name),
        escape_html(value)
    )
}

fn number_field(
    label: &str,
    name: &str,
    value: f64,
    minimum: f64,
    maximum: f64,
    step: &str,
) -> String {
    format!(
        "<label>{}<input type=\"number\" name=\"{}\" value=\"{}\" min=\"{}\" max=\"{}\" step=\"{}\" required></label>",
        escape_html(label),
        escape_html(name),
        value,
        minimum,
        maximum,
        step
    )
}

fn visualizer_delay_field(value: u64) -> String {
    format!(
        "<label><span id=\"visualizer-delay-label\">Visualizer delay ({} ms)</span>\
         <input id=\"visualizer-delay\" type=\"range\" name=\"visualizer_delay_ms\" value=\"{}\" min=\"0\" max=\"1500\" step=\"50\"></label>",
        value, value
    )
}

fn spotify_visualizer_delay_field(value: u64) -> String {
    format!(
        "<label><span id=\"spotify-visualizer-delay-label\">Spotify extra delay (+{} ms)</span>\
         <input id=\"spotify-visualizer-delay\" type=\"range\" name=\"spotify_visualizer_extra_delay_ms\" value=\"{}\" min=\"0\" max=\"1500\" step=\"50\"></label>",
        value, value
    )
}

fn carousel_speed_field(value: f64) -> String {
    let label = if value == 0.0 {
        "Carousel speed (paused)".to_string()
    } else {
        format!("Carousel speed ({value:.0} px/s)")
    };
    format!(
        "<label><span id=\"carousel-speed-label\">{}</span>\
         <input id=\"carousel-speed\" type=\"range\" name=\"carousel_speed\" value=\"{}\" min=\"0\" max=\"80\" step=\"1\"></label>",
        escape_html(&label),
        value
    )
}

fn select_field(label: &str, name: &str, selected: &str, options: &[(&str, &str)]) -> String {
    let mut html = format!(
        "<label>{}<select name=\"{}\">",
        escape_html(label),
        escape_html(name)
    );
    for (value, text) in options {
        let selected_attribute = if *value == selected { " selected" } else { "" };
        let _ = write!(
            html,
            "<option value=\"{}\"{}>{}</option>",
            escape_html(value),
            selected_attribute,
            escape_html(text)
        );
    }
    html.push_str("</select></label>");
    html
}

fn checkbox(label: &str, name: &str, checked: bool) -> String {
    format!(
        "<label class=\"check\"><input type=\"checkbox\" name=\"{}\"{}>{}</label>",
        escape_html(name),
        if checked { " checked" } else { "" },
        escape_html(label)
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn query_parameter<'a>(target: &'a str, name: &str) -> Option<&'a str> {
    let query = target.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

fn decoded_query_parameter(target: &str, name: &str) -> Option<String> {
    let query = target.split_once('?')?.1;
    url::form_urlencoded::parse(query.as_bytes())
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
}

fn generate_csrf_token() -> String {
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_err()
    {
        let fallback = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            ^ std::process::id() as u128;
        bytes = fallback.to_le_bytes();
    }
    bytes.iter().fold(String::new(), |mut token, byte| {
        let _ = write!(token, "{byte:02x}");
        token
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn respond_html<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    body: &str,
) -> Result<(), String> {
    respond(
        stream,
        status,
        reason,
        "text/html; charset=utf-8",
        body,
        &[],
    )
}

fn respond_text<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    body: &str,
) -> Result<(), String> {
    respond(
        stream,
        status,
        reason,
        "text/plain; charset=utf-8",
        body,
        &[],
    )
}

fn respond_json<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    body: &str,
) -> Result<(), String> {
    respond(
        stream,
        status,
        reason,
        "application/json; charset=utf-8",
        body,
        &[],
    )
}

fn respond_public_json<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    body: &str,
) -> Result<(), String> {
    respond(
        stream,
        status,
        reason,
        "application/json; charset=utf-8",
        body,
        &[("Access-Control-Allow-Origin", "*")],
    )
}

fn live_snapshot_json(
    playback_status: &Arc<Mutex<WebPlaybackStatus>>,
    queue: &Arc<Mutex<Vec<MopidyQueueTrack>>>,
) -> Result<String, String> {
    let playback = playback_status
        .lock()
        .map(|status| status.clone())
        .unwrap_or_default();
    let queue = queue.lock().map(|queue| queue.clone()).unwrap_or_default();
    serde_json::to_string(&WebLiveSnapshot { playback, queue })
        .map_err(|error| format!("could not serialize live state: {error}"))
}

struct ActiveClientGuard<'a>(&'a AtomicUsize);

impl Drop for ActiveClientGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn respond_event_stream<W: Write>(stream: &mut W, server: &WebServerState) -> Result<(), String> {
    server.active_clients.fetch_add(1, Ordering::Relaxed);
    let _active_client = ActiveClientGuard(&server.active_clients);
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        )
        .map_err(|error| error.to_string())?;
    let mut previous = String::new();
    for tick in 0..120 {
        let current = live_snapshot_json(&server.playback_status, &server.queue)?;
        if current != previous {
            write!(stream, "event: state\ndata: {current}\n\n")
                .map_err(|error| error.to_string())?;
            previous = current;
        } else if tick % 60 == 0 {
            stream
                .write_all(b": keepalive\n\n")
                .map_err(|error| error.to_string())?;
        }
        stream.flush().map_err(|error| error.to_string())?;
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

fn respond_script<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    body: &str,
) -> Result<(), String> {
    respond(
        stream,
        status,
        reason,
        "text/javascript; charset=utf-8",
        body,
        &[],
    )
}

fn respond_service_worker<W: Write>(stream: &mut W) -> Result<(), String> {
    respond(
        stream,
        200,
        "OK",
        "text/javascript; charset=utf-8",
        SERVICE_WORKER_JS,
        &[("Service-Worker-Allowed", "/")],
    )
}

fn respond_app_icon<W: Write>(stream: &mut W, size: u32) -> Result<(), String> {
    let png = generate_app_icon(size)?;
    respond_bytes(stream, 200, "OK", "image/png", &png, &[])
}

fn generate_app_icon(size: u32) -> Result<Vec<u8>, String> {
    let mut pixels = vec![0_u8; size as usize * size as usize * 4];
    for y in 0..size {
        for x in 0..size {
            let nx = (f64::from(x) + 0.5) / f64::from(size);
            let ny = (f64::from(y) + 0.5) / f64::from(size);
            let distance = ((nx - 0.5).powi(2) + (ny - 0.47).powi(2)).sqrt();
            let color = if (0.40..=0.63).contains(&nx) && (ny - 0.47).abs() <= (0.65 - nx) * 0.62 {
                [245, 245, 245, 255]
            } else if distance < 0.13 {
                [13, 14, 16, 255]
            } else if distance < 0.34 {
                [138, 180, 248, 255]
            } else if (0.755..=0.785).contains(&ny) && (0.25..=0.75).contains(&nx) {
                if nx < 0.58 {
                    [138, 180, 248, 255]
                } else {
                    [48, 49, 54, 255]
                }
            } else {
                [13, 14, 16, 255]
            };
            let offset = (y as usize * size as usize + x as usize) * 4;
            pixels[offset..offset + 4].copy_from_slice(&color);
        }
    }

    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    image::ImageEncoder::write_image(
        encoder,
        &pixels,
        size,
        size,
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|error| format!("could not encode application icon: {error}"))?;
    Ok(png)
}

fn respond_redirect<W: Write>(stream: &mut W, location: &str) -> Result<(), String> {
    respond(
        stream,
        303,
        "See Other",
        "text/plain; charset=utf-8",
        "Saved\n",
        &[("Location", location)],
    )
}

fn respond_session_redirect<W: Write>(
    stream: &mut W,
    location: &str,
    session: Option<&str>,
) -> Result<(), String> {
    let cookie = match session {
        Some(session) => format!(
            "orpheus_settings={session}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
            SETTINGS_SESSION_TTL.as_secs()
        ),
        None => "orpheus_settings=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0".to_string(),
    };
    respond(
        stream,
        303,
        "See Other",
        "text/plain; charset=utf-8",
        "Continue\n",
        &[("Location", location), ("Set-Cookie", &cookie)],
    )
}

fn respond<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
    extra_headers: &[(&str, &str)],
) -> Result<(), String> {
    respond_bytes(
        stream,
        status,
        reason,
        content_type,
        body.as_bytes(),
        extra_headers,
    )
}

fn respond_bytes<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> Result<(), String> {
    let mut headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'self'; worker-src 'self'; connect-src 'self'; img-src 'self'; manifest-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        let _ = write!(headers, "{name}: {value}\r\n");
    }
    headers.push_str("\r\n");
    stream
        .write_all(headers.as_bytes())
        .and_then(|()| stream.write_all(body))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::Cursor;

    fn test_library_search() -> Arc<LibrarySearch> {
        Arc::new(|query, limit| {
            Ok(vec![MopidySearchTrack {
                uri: "jellyfin:track:test-result".to_string(),
                title: format!("Result for {query}"),
                artist: "Test Artist".to_string(),
                album: "Test Album".to_string(),
                duration_ms: 185_000,
            }]
            .into_iter()
            .take(limit)
            .collect())
        })
    }

    fn test_library_browse() -> Arc<LibraryBrowse> {
        Arc::new(|kind, limit| {
            if !matches!(kind, "artists" | "albums") {
                return Err("unsupported browse kind".to_string());
            }
            Ok(vec![MopidyBrowseItem {
                uri: format!("jellyfin:{kind}:test-result"),
                name: format!("Test {kind}"),
                item_type: kind.trim_end_matches('s').to_string(),
            }]
            .into_iter()
            .take(limit)
            .collect())
        })
    }

    fn test_library_lookup() -> Arc<LibraryLookup> {
        Arc::new(|uri, limit| {
            if !(uri.starts_with("jellyfin:artist:") || uri.starts_with("jellyfin:album:")) {
                return Err("unsupported lookup URI".to_string());
            }
            Ok(vec![MopidySearchTrack {
                uri: "jellyfin:track:test-lookup".to_string(),
                title: format!("Track from {uri}"),
                artist: "Test Artist".to_string(),
                album: "Test Album".to_string(),
                duration_ms: 185_000,
            }]
            .into_iter()
            .take(limit)
            .collect())
        })
    }

    fn http_request_bytes(request: &str, playback: Arc<Mutex<WebPlaybackStatus>>) -> Vec<u8> {
        http_request_bytes_with_catalog(request, playback, Arc::new(Mutex::new(Vec::new())))
    }

    fn http_request_bytes_with_catalog(
        request: &str,
        playback: Arc<Mutex<WebPlaybackStatus>>,
        playlist_catalog: Arc<Mutex<Vec<WebPlaylist>>>,
    ) -> Vec<u8> {
        http_request_bytes_with_state(
            request,
            playback,
            playlist_catalog,
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
        )
    }

    fn http_request_bytes_with_state(
        request: &str,
        playback: Arc<Mutex<WebPlaybackStatus>>,
        playlist_catalog: Arc<Mutex<Vec<WebPlaylist>>>,
        queue: Arc<Mutex<Vec<MopidyQueueTrack>>>,
        history: Arc<Mutex<Vec<HistoryEntry>>>,
    ) -> Vec<u8> {
        let (updates, _received) = mpsc::channel();
        let mut parsed = read_request(&mut Cursor::new(request.as_bytes())).unwrap();
        parsed.headers.insert(
            "cookie".to_string(),
            "orpheus_settings=test-session".to_string(),
        );
        let mut response = Vec::new();
        let server = WebServerState {
            settings_path: PathBuf::from("/unused/settings.toml"),
            playlists_path: PathBuf::from("/unused/playlists.toml"),
            csrf_token: Arc::new("test-csrf-token".to_string()),
            updates,
            playback_status: playback,
            playlist_catalog,
            library_search: test_library_search(),
            library_browse: test_library_browse(),
            library_lookup: test_library_lookup(),
            recent_search_uris: Arc::new(Mutex::new(VecDeque::new())),
            settings_sessions: Arc::new(Mutex::new(HashMap::from([(
                "test-session".to_string(),
                Instant::now(),
            )]))),
            failed_logins: Arc::new(Mutex::new(VecDeque::new())),
            guest_queue_attempts: Arc::new(Mutex::new(HashMap::new())),
            queue_reservations: Arc::new(Mutex::new(HashMap::new())),
            queue_voters: Arc::new(Mutex::new(HashMap::new())),
            queue,
            history,
            active_clients: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
        };
        dispatch_request(&mut response, parsed, &server).unwrap();
        response
    }

    fn http_request(request: &str, playback: Arc<Mutex<WebPlaybackStatus>>) -> String {
        String::from_utf8(http_request_bytes(request, playback)).unwrap()
    }

    fn http_get(path: &str, playback: Arc<Mutex<WebPlaybackStatus>>) -> String {
        http_request(
            &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
            playback,
        )
    }

    fn response_parts(response: &str) -> (&str, &str) {
        response.split_once("\r\n\r\n").unwrap()
    }

    fn byte_response_parts(response: &[u8]) -> (&str, &[u8]) {
        let header_end = find_bytes(response, b"\r\n\r\n").unwrap();
        (
            std::str::from_utf8(&response[..header_end]).unwrap(),
            &response[header_end + 4..],
        )
    }

    #[test]
    fn settings_form_validates_and_maps_idle_mode() {
        let body = b"display_brightness=80&dim_timeout_seconds=30&dim_brightness=2&screensaver_brightness=25&carousel_speed=45&volume_step=5&max_volume=68&hardware_volume_ceiling_db=-8.5&visualizer_delay_ms=650&idle_mode=screensaver_dim&screensaver_style=clock&now_playing_style=visualizer&shuffle_playlists=on&weather_location=Example%20City&weather_latitude=-41.28664&weather_longitude=174.77557&morning_mode_enabled=on&morning_start_hour=7&morning_end_hour=10&morning_news_feed_url=https%3A%2F%2Fexample.com%2Fnews.xml&guest_mode_enabled=on&guest_queue_cooldown_seconds=120";
        let form = parse_form(body).unwrap();
        let settings = settings_from_form(Settings::default(), &form).unwrap();

        assert_eq!(settings.display_brightness, 0.8);
        assert!(settings.screensaver_enabled);
        assert!(settings.screensaver_heavy_dim);
        assert_eq!(settings.screensaver_style, ScreensaverStyle::Clock);
        assert_eq!(settings.now_playing_style, NowPlayingStyle::Visualizer);
        assert_eq!(settings.visualizer_delay_ms, 650);
        assert_eq!(settings.carousel_speed, 45.0);
        assert_eq!(settings.max_volume, 68);
        assert_eq!(settings.hardware_volume_ceiling_db, -8.5);
        assert_eq!(settings.weather_location, "Example City");
        assert!(settings.shuffle_playlists);
        assert!(!settings.shuffle_screensaver_art);
        assert!(settings.morning_mode_enabled);
        assert_eq!(settings.morning_start_hour, 7);
        assert_eq!(settings.morning_end_hour, 10);
        assert_eq!(
            settings.morning_news_feed_url,
            "https://example.com/news.xml"
        );
        assert!(settings.guest_mode_enabled);
        assert_eq!(settings.guest_queue_cooldown_seconds, 120);

        let mut track_info = form.clone();
        track_info.insert("now_playing_style".to_string(), "track_info".to_string());
        assert_eq!(
            settings_from_form(Settings::default(), &track_info)
                .unwrap()
                .now_playing_style,
            NowPlayingStyle::TrackInfo
        );

        let mut too_large = form;
        too_large.insert("visualizer_delay_ms".to_string(), "1550".to_string());
        assert!(settings_from_form(Settings::default(), &too_large).is_err());

        let mut too_fast = too_large;
        too_fast.insert("visualizer_delay_ms".to_string(), "1500".to_string());
        too_fast.insert("carousel_speed".to_string(), "81".to_string());
        assert!(settings_from_form(Settings::default(), &too_fast).is_err());
    }

    #[test]
    fn playlist_form_omits_empty_rows_and_rejects_partial_rows() {
        let valid = parse_form(b"playlist_name_0=Chill&playlist_uri_0=jellyfin%3Aplaylist%3A1&playlist_name_1=&playlist_uri_1=").unwrap();
        let config = playlists_from_form(&valid).unwrap();
        assert_eq!(config.playlists.len(), 1);
        assert_eq!(config.playlists[0].name, "Chill");

        let invalid = parse_form(b"playlist_name_0=Missing+URI").unwrap();
        assert!(playlists_from_form(&invalid).is_err());
    }

    #[test]
    fn playback_form_validates_actions_and_volume() {
        let (updates, received) = mpsc::channel();
        let request = |body: &str| HttpRequest {
            method: "POST".to_string(),
            target: "/playback".to_string(),
            headers: HashMap::from([(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )]),
            body: body.as_bytes().to_vec(),
        };

        send_playback_request(&request("csrf=token&action=toggle"), "token", &updates).unwrap();
        assert!(matches!(
            received.recv().unwrap(),
            WebConfigUpdate::Playback(WebPlaybackAction::TogglePlayPause)
        ));

        send_playback_request(
            &request("csrf=token&action=volume&volume=37"),
            "token",
            &updates,
        )
        .unwrap();
        assert!(matches!(
            received.recv().unwrap(),
            WebConfigUpdate::Playback(WebPlaybackAction::SetVolume(37))
        ));
        send_playback_request(
            &request("csrf=token&action=seek&position_seconds=123"),
            "token",
            &updates,
        )
        .unwrap();
        assert!(matches!(
            received.recv().unwrap(),
            WebConfigUpdate::Playback(WebPlaybackAction::Seek(123))
        ));
        send_playback_request(&request("csrf=token&action=screensaver"), "token", &updates)
            .unwrap();
        assert!(matches!(
            received.recv().unwrap(),
            WebConfigUpdate::Playback(WebPlaybackAction::ToggleScreensaver)
        ));
        assert!(
            send_playback_request(
                &request("csrf=token&action=volume&volume=101"),
                "token",
                &updates
            )
            .is_err()
        );
        assert!(
            send_playback_request(&request("csrf=wrong&action=next"), "token", &updates).is_err()
        );
        assert!(
            send_playback_request(&request("csrf=token&action=surprise"), "token", &updates)
                .is_err()
        );
    }

    #[test]
    fn jellyfin_search_route_decodes_queries_and_returns_tracks() {
        let response = http_get(
            "/api/search?q=Back+in+Black",
            Arc::new(Mutex::new(WebPlaybackStatus::default())),
        );
        let (headers, body) = response_parts(&response);
        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(headers.contains("Content-Type: application/json"));
        let tracks: Value = serde_json::from_str(body).unwrap();
        assert_eq!(tracks[0]["title"], "Result for Back in Black");
        assert_eq!(tracks[0]["uri"], "jellyfin:track:test-result");
        assert_eq!(tracks[0]["duration_ms"], 185_000);

        let too_short = http_get(
            "/api/search?q=ab",
            Arc::new(Mutex::new(WebPlaybackStatus::default())),
        );
        assert!(too_short.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    }

    #[test]
    fn queue_request_accepts_only_recent_jellyfin_search_results() {
        let (updates, received) = mpsc::channel();
        let recent = Arc::new(Mutex::new(VecDeque::from([
            "jellyfin:track:allowed".to_string()
        ])));
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reservations = Arc::new(Mutex::new(HashMap::new()));
        let request = |body: &str| HttpRequest {
            method: "POST".to_string(),
            target: "/queue".to_string(),
            headers: HashMap::from([(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )]),
            body: body.as_bytes().to_vec(),
        };

        let missing_name = send_queue_request(
            &request("csrf=token&track_uri=jellyfin%3Atrack%3Aallowed&placement=end"),
            "token",
            &updates,
            &recent,
            &queue,
            &reservations,
            None,
        )
        .unwrap_err();
        assert!(missing_name.contains("enter your name"));

        send_queue_request(
            &request("csrf=token&track_uri=jellyfin%3Atrack%3Aallowed&placement=next&requester_name=Alex"),
            "token",
            &updates,
            &recent,
            &queue,
            &reservations,
            None,
        )
        .unwrap();
        assert!(matches!(
            received.recv().unwrap(),
            WebConfigUpdate::QueueTrack {
                uri,
                placement: QueuePlacement::Next,
                requested_by: Some(requester),
            } if uri == "jellyfin:track:allowed" && requester == "Alex"
        ));
        let duplicate = send_queue_request(
            &request(
                "csrf=token&track_uri=jellyfin%3Atrack%3Aallowed&placement=end&requester_name=Sam",
            ),
            "token",
            &updates,
            &recent,
            &queue,
            &reservations,
            None,
        )
        .unwrap_err();
        assert!(duplicate.contains("already queued"));
        reservations.lock().unwrap().clear();
        queue.lock().unwrap().push(MopidyQueueTrack {
            tlid: 7,
            uri: "jellyfin:track:allowed".to_string(),
            title: "Already there".to_string(),
            artist: String::new(),
            album: String::new(),
            duration_ms: 1,
            current: false,
            requested_by: None,
            votes: 0,
        });
        assert!(
            send_queue_request(
                &request("csrf=token&track_uri=jellyfin%3Atrack%3Aallowed&placement=end&requester_name=Sam"),
                "token",
                &updates,
                &recent,
                &queue,
                &reservations,
                None,
            )
            .unwrap_err()
            .contains("already queued")
        );
        assert!(
            send_queue_request(
                &request("csrf=token&track_uri=jellyfin%3Atrack%3Aunknown&placement=end"),
                "token",
                &updates,
                &recent,
                &queue,
                &reservations,
                None,
            )
            .is_err()
        );
        assert!(
            send_queue_request(
                &request("csrf=token&track_uri=spotify%3Atrack%3Aallowed&placement=now"),
                "token",
                &updates,
                &recent,
                &queue,
                &reservations,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn guest_queue_additions_are_end_only_and_rate_limited_per_client() {
        let (updates, received) = mpsc::channel();
        let recent = Arc::new(Mutex::new(VecDeque::from([
            "jellyfin:track:one".to_string(),
            "jellyfin:track:two".to_string(),
            "jellyfin:track:three".to_string(),
            "jellyfin:track:four".to_string(),
            "jellyfin:track:five".to_string(),
        ])));
        let attempts = Arc::new(Mutex::new(HashMap::new()));
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reservations = Arc::new(Mutex::new(HashMap::new()));
        let request = |track: &str, placement: &str| {
            HttpRequest {
            method: "POST".to_string(),
            target: "/queue".to_string(),
            headers: HashMap::from([(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )]),
            body: format!(
                "csrf=token&track_uri=jellyfin%3Atrack%3A{track}&placement={placement}&requester_name=Alex"
            )
            .into_bytes(),
        }
        };
        let policy = |client_id| GuestQueuePolicy {
            client_id,
            cooldown: Duration::from_secs(120),
            attempts: &attempts,
        };

        send_queue_request(
            &request("one", "end"),
            "token",
            &updates,
            &recent,
            &queue,
            &reservations,
            Some(policy("living-room")),
        )
        .unwrap();
        assert!(matches!(
            received.recv().unwrap(),
            WebConfigUpdate::QueueTrack {
                placement: QueuePlacement::End,
                requested_by: Some(requester),
                ..
            } if requester == "Alex"
        ));

        let repeated = send_queue_request(
            &request("two", "end"),
            "token",
            &updates,
            &recent,
            &queue,
            &reservations,
            Some(policy("living-room")),
        )
        .unwrap_err();
        assert!(repeated.contains("cooldown active"));

        send_queue_request(
            &request("three", "end"),
            "token",
            &updates,
            &recent,
            &queue,
            &reservations,
            Some(policy("kitchen")),
        )
        .unwrap();
        assert!(
            send_queue_request(
                &request("four", "next"),
                "token",
                &updates,
                &recent,
                &queue,
                &reservations,
                Some(policy("garden")),
            )
            .unwrap_err()
            .contains("only add songs to the end")
        );

        // An authenticated controller is deliberately not subject to the guest policy.
        send_queue_request(
            &request("five", "next"),
            "token",
            &updates,
            &recent,
            &queue,
            &reservations,
            None,
        )
        .unwrap();
    }

    #[test]
    fn queue_votes_are_limited_to_one_per_client_and_upcoming_tracks() {
        let (updates, received) = mpsc::channel();
        let track = |tlid, current| MopidyQueueTrack {
            tlid,
            uri: format!("jellyfin:track:{tlid}"),
            title: format!("Track {tlid}"),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            duration_ms: 1,
            current,
            requested_by: None,
            votes: 0,
        };
        let queue = Arc::new(Mutex::new(vec![track(1, true), track(2, false)]));
        let voters = Arc::new(Mutex::new(HashMap::new()));
        let request = |tlid| HttpRequest {
            method: "POST".to_string(),
            target: "/queue/vote".to_string(),
            headers: HashMap::from([(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )]),
            body: format!("csrf=token&tlid={tlid}").into_bytes(),
        };

        send_queue_vote_request(
            &request(2),
            "token",
            &updates,
            &queue,
            &voters,
            "living-room",
        )
        .unwrap();
        assert!(matches!(
            received.recv().unwrap(),
            WebConfigUpdate::QueueVote(2)
        ));
        assert!(
            send_queue_vote_request(
                &request(2),
                "token",
                &updates,
                &queue,
                &voters,
                "living-room",
            )
            .unwrap_err()
            .contains("already voted")
        );
        send_queue_vote_request(&request(2), "token", &updates, &queue, &voters, "kitchen")
            .unwrap();
        assert!(matches!(
            received.recv().unwrap(),
            WebConfigUpdate::QueueVote(2)
        ));
        assert!(
            send_queue_vote_request(&request(1), "token", &updates, &queue, &voters, "kitchen",)
                .unwrap_err()
                .contains("only upcoming")
        );
    }

    #[test]
    fn queue_editor_maps_all_actions_and_rejects_invalid_ids() {
        let (updates, received) = mpsc::channel();
        let request = |body: &str| HttpRequest {
            method: "POST".to_string(),
            target: "/queue/edit".to_string(),
            headers: HashMap::from([(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )]),
            body: body.as_bytes().to_vec(),
        };
        for (action, expected) in [
            ("remove", WebQueueAction::Remove(42)),
            ("up", WebQueueAction::MoveUp(42)),
            ("down", WebQueueAction::MoveDown(42)),
            ("play", WebQueueAction::Play(42)),
        ] {
            send_queue_edit_request(
                &request(&format!("csrf=token&action={action}&tlid=42")),
                "token",
                &updates,
            )
            .unwrap();
            assert!(matches!(
                received.recv().unwrap(),
                WebConfigUpdate::QueueEdit(actual) if actual == expected
            ));
        }
        send_queue_edit_request(&request("csrf=token&action=clear"), "token", &updates).unwrap();
        assert!(matches!(
            received.recv().unwrap(),
            WebConfigUpdate::QueueEdit(WebQueueAction::Clear)
        ));
        assert!(
            send_queue_edit_request(
                &request("csrf=token&action=remove&tlid=0"),
                "token",
                &updates,
            )
            .is_err()
        );
    }

    #[test]
    fn browse_and_lookup_routes_return_library_models() {
        let playback = Arc::new(Mutex::new(WebPlaybackStatus::default()));
        let browse = http_get("/api/browse?kind=artists", Arc::clone(&playback));
        let (browse_headers, browse_body) = response_parts(&browse);
        assert!(browse_headers.starts_with("HTTP/1.1 200 OK\r\n"));
        let items: Value = serde_json::from_str(browse_body).unwrap();
        assert_eq!(items[0]["name"], "Test artists");
        assert_eq!(items[0]["item_type"], "artist");

        let lookup = http_get(
            "/api/lookup?uri=jellyfin%3Aalbum%3Atest-result",
            Arc::clone(&playback),
        );
        let (_, lookup_body) = response_parts(&lookup);
        let tracks: Value = serde_json::from_str(lookup_body).unwrap();
        assert_eq!(tracks[0]["uri"], "jellyfin:track:test-lookup");
        assert!(
            tracks[0]["title"]
                .as_str()
                .unwrap()
                .contains("jellyfin:album:test-result")
        );

        let invalid = http_get("/api/browse?kind=genres", playback);
        assert!(invalid.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
    }

    #[test]
    fn settings_login_creates_a_private_session_without_rendering_the_password() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("orpheus-auth-{}-{unique}.toml", std::process::id()));
        let settings = Settings {
            web_settings_password: "correct-horse-battery".to_string(),
            ..Settings::default()
        };
        settings.save(&path).unwrap();
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let failures = Arc::new(Mutex::new(VecDeque::new()));
        let request = |password: &str| HttpRequest {
            method: "POST".to_string(),
            target: "/login".to_string(),
            headers: HashMap::from([(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )]),
            body: format!("csrf=token&password={password}").into_bytes(),
        };

        assert_eq!(
            login_request(&request("wrong"), &path, "token", &sessions, &failures),
            Err(LoginError::Rejected)
        );
        let session = login_request(
            &request("correct-horse-battery"),
            &path,
            "token",
            &sessions,
            &failures,
        )
        .unwrap();
        let authenticated = HttpRequest {
            method: "GET".to_string(),
            target: "/settings".to_string(),
            headers: HashMap::from([(
                "cookie".to_string(),
                format!("other=x; orpheus_settings={session}"),
            )]),
            body: Vec::new(),
        };
        assert!(settings_request_is_authenticated(&authenticated, &sessions));
        let html = render_settings_page(&settings, &Config::default(), "token", None);
        assert!(!html.contains("correct-horse-battery"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn configuration_routes_redirect_guests_but_playback_stays_public() {
        let (updates, _received) = mpsc::channel();
        let server = WebServerState {
            settings_path: PathBuf::from("/unused/settings.toml"),
            playlists_path: PathBuf::from("/unused/playlists.toml"),
            csrf_token: Arc::new("test-csrf-token".to_string()),
            updates,
            playback_status: Arc::new(Mutex::new(WebPlaybackStatus::default())),
            playlist_catalog: Arc::new(Mutex::new(Vec::new())),
            library_search: test_library_search(),
            library_browse: test_library_browse(),
            library_lookup: test_library_lookup(),
            recent_search_uris: Arc::new(Mutex::new(VecDeque::new())),
            settings_sessions: Arc::new(Mutex::new(HashMap::new())),
            failed_logins: Arc::new(Mutex::new(VecDeque::new())),
            guest_queue_attempts: Arc::new(Mutex::new(HashMap::new())),
            queue_reservations: Arc::new(Mutex::new(HashMap::new())),
            queue_voters: Arc::new(Mutex::new(HashMap::new())),
            queue: Arc::new(Mutex::new(Vec::new())),
            history: Arc::new(Mutex::new(Vec::new())),
            active_clients: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
        };
        let dispatch = |target: &str| {
            let request = HttpRequest {
                method: "GET".to_string(),
                target: target.to_string(),
                headers: HashMap::new(),
                body: Vec::new(),
            };
            let mut response = Vec::new();
            dispatch_request(&mut response, request, &server).unwrap();
            String::from_utf8(response).unwrap()
        };

        let settings = dispatch("/settings");
        assert!(settings.starts_with("HTTP/1.1 303 See Other\r\n"));
        assert!(settings.contains("Location: /login\r\n"));
        assert!(dispatch("/history").starts_with("HTTP/1.1 303 See Other\r\n"));
        assert!(dispatch("/diagnostics").starts_with("HTTP/1.1 303 See Other\r\n"));
        assert!(dispatch("/api/history").starts_with("HTTP/1.1 303 See Other\r\n"));
        assert!(dispatch("/").starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(dispatch("/login").contains("Unlock configuration"));
    }

    #[test]
    fn guest_mode_allows_play_pause_and_volume_but_keeps_privileged_controls_private() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let settings_path = std::env::temp_dir().join(format!(
            "orpheus-guest-{}-{unique}.toml",
            std::process::id()
        ));
        Settings {
            guest_mode_enabled: true,
            guest_queue_cooldown_seconds: 120,
            ..Settings::default()
        }
        .save(&settings_path)
        .unwrap();
        let (updates, received) = mpsc::channel();
        let server = WebServerState {
            settings_path: settings_path.clone(),
            playlists_path: PathBuf::from("/unused/playlists.toml"),
            csrf_token: Arc::new("test-csrf-token".to_string()),
            updates,
            playback_status: Arc::new(Mutex::new(WebPlaybackStatus {
                online: true,
                mopidy_online: true,
                ..WebPlaybackStatus::default()
            })),
            playlist_catalog: Arc::new(Mutex::new(Vec::new())),
            library_search: test_library_search(),
            library_browse: test_library_browse(),
            library_lookup: test_library_lookup(),
            recent_search_uris: Arc::new(Mutex::new(VecDeque::from([
                "jellyfin:track:allowed".to_string(),
                "jellyfin:track:second".to_string(),
            ]))),
            settings_sessions: Arc::new(Mutex::new(HashMap::new())),
            failed_logins: Arc::new(Mutex::new(VecDeque::new())),
            guest_queue_attempts: Arc::new(Mutex::new(HashMap::new())),
            queue_reservations: Arc::new(Mutex::new(HashMap::new())),
            queue_voters: Arc::new(Mutex::new(HashMap::new())),
            queue: Arc::new(Mutex::new(Vec::new())),
            history: Arc::new(Mutex::new(Vec::new())),
            active_clients: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
        };

        let mut page = Vec::new();
        dispatch_request_for_client(
            &mut page,
            HttpRequest {
                method: "GET".to_string(),
                target: "/".to_string(),
                headers: HashMap::new(),
                body: Vec::new(),
            },
            &server,
            "192.0.2.10",
        )
        .unwrap();
        let page = String::from_utf8(page).unwrap();
        assert!(page.contains("id=\"guest-mode\""));
        assert!(page.contains("one song every 120 seconds"));
        assert!(!page.contains("id=\"quick-playlist-form\""));
        assert!(!page.contains("id=\"queue-clear\""));
        assert!(page.contains("value=\"previous\" disabled>Previous"));
        assert!(page.contains("value=\"toggle\">Play"));
        assert!(page.contains("id=\"playback-volume\" data-playback-control type=\"range\""));
        assert!(
            !page.contains("id=\"playback-volume\" data-playback-control type=\"range\" disabled")
        );
        assert!(page.contains("value=\"next\" disabled>Next"));
        assert!(page.contains("value=\"screensaver\" disabled>Preview screensaver"));

        let post = |track: &str| {
            HttpRequest {
            method: "POST".to_string(),
            target: "/queue".to_string(),
            headers: HashMap::from([
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("accept".to_string(), "application/json".to_string()),
            ]),
            body: format!("csrf=test-csrf-token&track_uri=jellyfin%3Atrack%3A{track}&placement=end&requester_name=Alex").into_bytes(),
        }
        };
        let mut first = Vec::new();
        dispatch_request_for_client(&mut first, post("allowed"), &server, "192.0.2.10").unwrap();
        assert!(
            String::from_utf8(first)
                .unwrap()
                .starts_with("HTTP/1.1 202 Accepted\r\n")
        );
        assert!(matches!(
            received.recv().unwrap(),
            WebConfigUpdate::QueueTrack {
                placement: QueuePlacement::End,
                ..
            }
        ));

        let mut repeated = Vec::new();
        dispatch_request_for_client(&mut repeated, post("second"), &server, "192.0.2.10").unwrap();
        assert!(
            String::from_utf8(repeated)
                .unwrap()
                .starts_with("HTTP/1.1 429 Too Many Requests\r\n")
        );

        let playback_post = |action: &str| HttpRequest {
            method: "POST".to_string(),
            target: "/playback".to_string(),
            headers: HashMap::from([
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("accept".to_string(), "application/json".to_string()),
            ]),
            body: format!("csrf=test-csrf-token&{action}").into_bytes(),
        };

        let mut toggle = Vec::new();
        dispatch_request_for_client(
            &mut toggle,
            playback_post("action=toggle"),
            &server,
            "192.0.2.10",
        )
        .unwrap();
        assert!(
            String::from_utf8(toggle)
                .unwrap()
                .starts_with("HTTP/1.1 202 Accepted\r\n")
        );
        assert!(matches!(
            received.recv().unwrap(),
            WebConfigUpdate::Playback(WebPlaybackAction::TogglePlayPause)
        ));

        let mut volume = Vec::new();
        dispatch_request_for_client(
            &mut volume,
            playback_post("action=volume&volume=37"),
            &server,
            "192.0.2.10",
        )
        .unwrap();
        assert!(
            String::from_utf8(volume)
                .unwrap()
                .starts_with("HTTP/1.1 202 Accepted\r\n")
        );
        assert!(matches!(
            received.recv().unwrap(),
            WebConfigUpdate::Playback(WebPlaybackAction::SetVolume(37))
        ));

        let mut next = Vec::new();
        dispatch_request_for_client(
            &mut next,
            playback_post("action=next"),
            &server,
            "192.0.2.10",
        )
        .unwrap();
        assert!(
            String::from_utf8(next)
                .unwrap()
                .starts_with("HTTP/1.1 403 Forbidden\r\n")
        );

        std::fs::remove_file(settings_path).unwrap();
    }

    #[test]
    fn rendered_values_are_html_escaped() {
        let settings = Settings {
            weather_location: "<Example City & nearby>".to_string(),
            ..Settings::default()
        };
        let playback = WebPlaybackStatus {
            title: "Song <One>".to_string(),
            artist: "Artist & Friends".to_string(),
            album: "An \"Album\"".to_string(),
            is_playing: true,
            online: true,
            source: "Mopidy".to_string(),
            mopidy_online: true,
            spotifyd_available: false,
            volume: Some(37),
            max_volume: 70,
            position_seconds: 61.0,
            duration_seconds: 185.0,
            next_title: Some("Next <Song>".to_string()),
            next_artist: Some("Next & Artist".to_string()),
            artwork_id: Some("art<&>".to_string()),
            artwork_path: Some("/private/should-not-appear.jpg".to_string()),
            screensaver_active: true,
            carousel_cover_count: 12,
            carousel_speed: 35.0,
        };
        let player_html = render_player_page(&playback, &[], "token", None, false, 120);
        let settings_html = render_settings_page(&settings, &Config::default(), "token", None);

        assert!(settings_html.contains("&lt;Example City &amp; nearby&gt;"));
        assert!(!settings_html.contains("value=\"<Example City"));
        assert!(player_html.contains("Song &lt;One&gt;"));
        assert!(player_html.contains("Artist &amp; Friends"));
        assert!(player_html.contains("An &quot;Album&quot;"));
        assert!(player_html.contains("Next &amp; Artist — Next &lt;Song&gt;"));
        assert!(player_html.contains("data-artwork-id=\"art&lt;&amp;&gt;\""));
        assert!(!player_html.contains("/private/should-not-appear.jpg"));
        assert!(player_html.contains(">Pause</button>"));
        assert!(player_html.contains("Volume (37%)"));
        assert!(player_html.contains("max=\"70\""));
        assert!(player_html.contains(">Wake display</button>"));
        assert!(player_html.contains("12 album covers ready · 35 px/s"));
        assert!(player_html.contains("id=\"playback-progress\""));
        assert!(player_html.contains("src=\"/app.js?v=9\""));
        assert!(player_html.contains("id=\"identity-gate\""));
        assert!(player_html.contains("id=\"client-name-label\""));
        assert!(player_html.contains("<main inert"));
        assert!(settings_html.contains("id=\"identity-gate\""));
        assert!(settings_html.contains("src=\"/identity.js?v=1\""));
        assert!(settings_html.contains("<main inert>"));
        assert!(player_html.contains("id=\"library-search-form\""));
        assert!(player_html.contains("id=\"library-search-results\""));
        assert_eq!(player_html.matches("<form").count(), 4);
        assert_eq!(player_html.matches("</form>").count(), 4);
        assert_eq!(settings_html.matches("<form").count(), 4);
        assert_eq!(settings_html.matches("</form>").count(), 4);
        let settings_start = settings_html
            .find("<form id=\"settings-form\" method=\"post\" action=\"/settings\"")
            .unwrap();
        let settings_form = settings_html[settings_start..]
            .split("</form>")
            .next()
            .unwrap();
        assert!(settings_form.contains("name=\"weather_location\""));
        assert!(settings_form.contains("name=\"visualizer_delay_ms\""));
        assert!(settings_form.contains("name=\"max_volume\""));
        assert!(settings_form.contains("name=\"hardware_volume_ceiling_db\""));
        assert!(settings_form.contains("name=\"carousel_speed\""));
        assert!(settings_form.contains("name=\"morning_mode_enabled\""));
        assert!(settings_form.contains("name=\"morning_news_feed_url\""));
        assert!(settings_form.contains("name=\"guest_mode_enabled\""));
        assert!(settings_form.contains("name=\"guest_queue_cooldown_seconds\""));
        assert!(settings_form.contains("name=\"new_web_settings_password\""));
        assert!(settings_form.contains("max=\"80\" step=\"1\""));
        assert!(settings_form.contains("max=\"1500\" step=\"50\""));
        assert!(settings_form.contains("name=\"csrf\" value=\"token\""));
    }

    #[test]
    fn playback_status_serializes_for_live_updates() {
        let status = WebPlaybackStatus {
            title: "Live track".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            is_playing: true,
            online: true,
            source: "Spotify Connect".to_string(),
            mopidy_online: true,
            spotifyd_available: true,
            volume: Some(82),
            max_volume: 85,
            position_seconds: 15.5,
            duration_seconds: 120.0,
            next_title: Some("Next track".to_string()),
            next_artist: None,
            artwork_id: Some("abc123".to_string()),
            artwork_path: Some("/tmp/private-art.jpg".to_string()),
            screensaver_active: true,
            carousel_cover_count: 12,
            carousel_speed: 28.0,
        };
        let value = serde_json::to_value(status).unwrap();

        assert_eq!(value["title"], "Live track");
        assert_eq!(value["source"], "Spotify Connect");
        assert_eq!(value["mopidy_online"], true);
        assert_eq!(value["spotifyd_available"], true);
        assert_eq!(value["max_volume"], 85);
        assert_eq!(value["position_seconds"], 15.5);
        assert_eq!(value["next_title"], "Next track");
        assert_eq!(value["artwork_id"], "abc123");
        assert_eq!(value["screensaver_active"], true);
        assert_eq!(value["carousel_cover_count"], 12);
        assert_eq!(value["carousel_speed"], 28.0);
        assert!(value.get("artwork_path").is_none());
        assert!(value["next_artist"].is_null());
    }

    #[test]
    fn live_snapshot_contains_playback_and_social_queue_metadata() {
        let playback = Arc::new(Mutex::new(WebPlaybackStatus {
            title: "Synced track".to_string(),
            ..WebPlaybackStatus::default()
        }));
        let queue = Arc::new(Mutex::new(vec![MopidyQueueTrack {
            tlid: 42,
            uri: "jellyfin:track:synced".to_string(),
            title: "Queued track".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            duration_ms: 1,
            current: false,
            requested_by: Some("Alex".to_string()),
            votes: 4,
        }]));

        let value: Value =
            serde_json::from_str(&live_snapshot_json(&playback, &queue).unwrap()).unwrap();
        assert_eq!(value["playback"]["title"], "Synced track");
        assert_eq!(value["queue"][0]["tlid"], 42);
        assert_eq!(value["queue"][0]["requested_by"], "Alex");
        assert_eq!(value["queue"][0]["votes"], 4);
    }

    #[test]
    fn playlist_catalog_endpoint_supports_the_remote_picker() {
        let catalog = Arc::new(Mutex::new(vec![
            WebPlaylist {
                name: "Pinned favourite".to_string(),
                uri: "spotify:playlist:pinned".to_string(),
                favorite: true,
            },
            WebPlaylist {
                name: "Library playlist".to_string(),
                uri: "jellyfin:playlist:library".to_string(),
                favorite: false,
            },
        ]));
        let response = String::from_utf8(http_request_bytes_with_catalog(
            "GET /api/playlists HTTP/1.1\r\nHost: localhost\r\n\r\n",
            Arc::new(Mutex::new(WebPlaybackStatus::default())),
            catalog,
        ))
        .unwrap();
        let (headers, body) = response_parts(&response);
        let playlists: Value = serde_json::from_str(body).unwrap();

        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert_eq!(playlists.as_array().unwrap().len(), 2);
        assert_eq!(playlists[0]["name"], "Pinned favourite");
        assert_eq!(playlists[0]["favorite"], true);
        assert_eq!(playlists[1]["uri"], "jellyfin:playlist:library");
    }

    #[test]
    fn queue_and_history_endpoints_publish_the_latest_shared_state() {
        let queue = Arc::new(Mutex::new(vec![MopidyQueueTrack {
            tlid: 44,
            uri: "jellyfin:track:queued".to_string(),
            title: "Queued song".to_string(),
            artist: "Queue artist".to_string(),
            album: "Queue album".to_string(),
            duration_ms: 240_000,
            current: true,
            requested_by: Some("Alex".to_string()),
            votes: 2,
        }]));
        let history = Arc::new(Mutex::new(vec![HistoryEntry {
            id: "history-one".to_string(),
            uri: Some("jellyfin:track:played".to_string()),
            title: "Played song".to_string(),
            artist: "History artist".to_string(),
            album: "History album".to_string(),
            source: "Mopidy".to_string(),
            duration_ms: 180_000,
            played_at_unix: 1_700_000_000,
        }]));
        let playback = Arc::new(Mutex::new(WebPlaybackStatus::default()));
        let playlists = Arc::new(Mutex::new(Vec::new()));

        let queue_response = String::from_utf8(http_request_bytes_with_state(
            "GET /api/queue HTTP/1.1\r\nHost: localhost\r\n\r\n",
            Arc::clone(&playback),
            Arc::clone(&playlists),
            Arc::clone(&queue),
            Arc::clone(&history),
        ))
        .unwrap();
        let (_, queue_body) = response_parts(&queue_response);
        let queue_json: Value = serde_json::from_str(queue_body).unwrap();
        assert_eq!(queue_json[0]["tlid"], 44);
        assert_eq!(queue_json[0]["title"], "Queued song");
        assert_eq!(queue_json[0]["current"], true);
        assert_eq!(queue_json[0]["requested_by"], "Alex");
        assert_eq!(queue_json[0]["votes"], 2);

        let history_response = String::from_utf8(http_request_bytes_with_state(
            "GET /api/history HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\n\r\n",
            playback,
            playlists,
            queue,
            history,
        ))
        .unwrap();
        let (_, history_body) = response_parts(&history_response);
        let history_json: Value = serde_json::from_str(history_body).unwrap();
        assert_eq!(history_json[0]["title"], "Played song");
        assert_eq!(history_json[0]["source"], "Mopidy");
    }

    #[test]
    fn history_and_diagnostics_pages_escape_content_and_avoid_secrets() {
        let history = vec![HistoryEntry {
            id: "safe-id".to_string(),
            uri: Some("jellyfin:track:safe".to_string()),
            title: "Song <script>".to_string(),
            artist: "Artist & guest".to_string(),
            album: "Album".to_string(),
            source: "Mopidy".to_string(),
            duration_ms: 1,
            played_at_unix: 1_700_000_000,
        }];
        let history_html = render_history_page(&history, "unused-secret");
        assert!(history_html.contains("Song &lt;script&gt;"));
        assert!(history_html.contains("Artist &amp; guest"));
        assert!(!history_html.contains("unused-secret"));

        let diagnostics = WebDiagnostics {
            app_uptime_seconds: 3_661,
            system_uptime_seconds: 90_000,
            cpu_temperature_c: Some(42.25),
            source: "Mopidy".to_string(),
            playback_online: true,
            mopidy_online: true,
            spotifyd_available: false,
            now_playing_style: "Visualizer".to_string(),
            queue_tracks: 3,
            history_entries: 9,
            artwork_cache_files: 12,
            artwork_cache_bytes: 1_048_576,
            persistent_queue_file: true,
        };
        let diagnostics_html = render_diagnostics_page(&diagnostics);
        assert!(diagnostics_html.contains("42.2 °C"));
        assert!(diagnostics_html.contains("1h 1m"));
        assert!(diagnostics_html.contains("3 tracks"));
        assert!(diagnostics_html.contains("1.0 MiB"));
        assert!(diagnostics_html.contains("Persistent queue"));
    }

    #[test]
    fn catalog_playlist_selection_is_validated_server_side() {
        let catalog = Arc::new(Mutex::new(vec![WebPlaylist {
            name: "Trusted name".to_string(),
            uri: "spotify:playlist:trusted".to_string(),
            favorite: false,
        }]));
        let request = |body: &str| HttpRequest {
            method: "POST".to_string(),
            target: "/library/play".to_string(),
            headers: HashMap::from([(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )]),
            body: body.as_bytes().to_vec(),
        };
        let (updates, received) = mpsc::channel();

        send_catalog_playlist_request(
            &request("csrf=token&playlist_uri=spotify%3Aplaylist%3Atrusted"),
            "token",
            &updates,
            &catalog,
        )
        .unwrap();
        let WebConfigUpdate::PlayPlaylist(playlist) = received.recv().unwrap() else {
            panic!("expected playlist playback update");
        };
        assert_eq!(playlist.name, "Trusted name");
        assert_eq!(playlist.uri, "spotify:playlist:trusted");
        assert!(playlist.art_uri.is_none());

        assert!(
            send_catalog_playlist_request(
                &request("csrf=token&playlist_uri=spotify%3Aplaylist%3Aunknown"),
                "token",
                &updates,
                &catalog,
            )
            .is_err()
        );
    }

    #[test]
    fn playlist_launcher_is_searchable_and_escapes_catalog_values() {
        let playlist = WebPlaylist {
            name: "Rock <&> Roll".to_string(),
            uri: "test:playlist:&danger".to_string(),
            favorite: true,
        };
        let html = playlist_launcher(&[playlist], true, "token");

        assert!(html.contains("id=\"playlist-filter\""));
        assert!(html.contains("id=\"playlist-select\""));
        assert!(html.contains("action=\"/library/play\""));
        assert!(html.contains("★ Rock &lt;&amp;&gt; Roll"));
        assert!(html.contains("value=\"test:playlist:&amp;danger\""));
        assert!(!html.contains("Rock <&> Roll"));
    }

    #[test]
    fn live_status_endpoint_reads_the_latest_shared_state() {
        let playback = Arc::new(Mutex::new(WebPlaybackStatus {
            title: "First track".to_string(),
            artist: "First artist".to_string(),
            album: "First album".to_string(),
            is_playing: true,
            online: true,
            source: "Mopidy".to_string(),
            mopidy_online: true,
            spotifyd_available: false,
            volume: Some(55),
            max_volume: 70,
            position_seconds: 10.0,
            duration_seconds: 100.0,
            next_title: Some("First next".to_string()),
            next_artist: Some("Next artist".to_string()),
            artwork_id: Some("first-art".to_string()),
            artwork_path: Some("/tmp/first-art.jpg".to_string()),
            screensaver_active: false,
            carousel_cover_count: 8,
            carousel_speed: 20.0,
        }));

        let first_response = http_get("/api/status", Arc::clone(&playback));
        let (first_headers, first_body) = response_parts(&first_response);
        assert!(first_headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(first_headers.contains("Content-Type: application/json; charset=utf-8"));
        assert!(first_headers.contains("Cache-Control: no-store"));
        let first: Value = serde_json::from_str(first_body).unwrap();
        assert_eq!(first["title"], "First track");
        assert_eq!(first["position_seconds"], 10.0);

        *playback.lock().unwrap() = WebPlaybackStatus {
            title: "Second track".to_string(),
            artist: "Second artist".to_string(),
            album: "Second album".to_string(),
            is_playing: false,
            online: true,
            source: "Spotify Connect".to_string(),
            mopidy_online: true,
            spotifyd_available: true,
            volume: Some(72),
            max_volume: 80,
            position_seconds: 42.25,
            duration_seconds: 180.0,
            next_title: None,
            next_artist: None,
            artwork_id: None,
            artwork_path: None,
            screensaver_active: true,
            carousel_cover_count: 11,
            carousel_speed: 40.0,
        };

        let second_response = http_get("/api/status", playback);
        let (second_headers, second_body) = response_parts(&second_response);
        let content_length = second_headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert_eq!(content_length, second_body.len());
        let second: Value = serde_json::from_str(second_body).unwrap();
        assert_eq!(second["title"], "Second track");
        assert_eq!(second["position_seconds"], 42.25);
        assert_eq!(second["volume"], 72);
        assert_eq!(second["is_playing"], false);
        assert_eq!(second["screensaver_active"], true);
        assert_eq!(second["carousel_cover_count"], 11);
        assert!(second["next_title"].is_null());
    }

    #[test]
    fn public_status_summary_is_grafana_friendly_and_excludes_private_data() {
        let playback = Arc::new(Mutex::new(WebPlaybackStatus {
            title: "Space Lion".to_string(),
            artist: "The Seatbelts".to_string(),
            album: "Cowboy Bebop".to_string(),
            is_playing: true,
            online: true,
            source: "Mopidy".to_string(),
            mopidy_online: true,
            spotifyd_available: true,
            volume: Some(44),
            max_volume: 80,
            position_seconds: 183.421,
            duration_seconds: 427.0,
            next_title: Some("Blue".to_string()),
            next_artist: Some("The Seatbelts".to_string()),
            artwork_id: Some("public-art-id".to_string()),
            artwork_path: Some("/tmp/private-cover.jpg".to_string()),
            screensaver_active: false,
            carousel_cover_count: 13,
            carousel_speed: 35.0,
        }));
        let queue = Arc::new(Mutex::new(vec![
            MopidyQueueTrack {
                tlid: 1,
                uri: "jellyfin:track:private-current".to_string(),
                title: "Space Lion".to_string(),
                artist: "The Seatbelts".to_string(),
                album: "Cowboy Bebop".to_string(),
                duration_ms: 427_000,
                current: true,
                requested_by: Some("Private requester".to_string()),
                votes: 2,
            },
            MopidyQueueTrack {
                tlid: 2,
                uri: "jellyfin:track:private-next".to_string(),
                title: "Blue".to_string(),
                artist: "The Seatbelts".to_string(),
                album: "Cowboy Bebop".to_string(),
                duration_ms: 200_000,
                current: false,
                requested_by: Some("Another requester".to_string()),
                votes: 3,
            },
        ]));
        let history = Arc::new(Mutex::new(vec![HistoryEntry {
            id: "private-history-id".to_string(),
            uri: Some("jellyfin:track:private-history".to_string()),
            title: "Earlier".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            source: "Mopidy".to_string(),
            duration_ms: 1,
            played_at_unix: 1,
        }]));
        let response = String::from_utf8(http_request_bytes_with_state(
            "GET /api/status/summary HTTP/1.1\r\nHost: localhost\r\n\r\n",
            playback,
            Arc::new(Mutex::new(Vec::new())),
            queue,
            history,
        ))
        .unwrap();
        let (headers, body) = response_parts(&response);
        let status: Value = serde_json::from_str(body).unwrap();

        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(headers.contains("Access-Control-Allow-Origin: *"));
        assert!(headers.contains("Cache-Control: no-store"));
        assert_eq!(status["schema_version"], 1);
        assert_eq!(status["healthy"], true);
        assert_eq!(status["state"], "playing");
        assert_eq!(status["track"]["title"], "Space Lion");
        assert_eq!(status["track"]["duration_ms"], 427_000);
        assert_eq!(status["position_ms"], 183_421);
        assert_eq!(status["progress_percent"], 42.96);
        assert_eq!(status["volume_percent"], 44);
        assert_eq!(status["clients"], 0);
        assert_eq!(status["queue_length"], 2);
        assert_eq!(status["queue_upcoming"], 1);
        assert_eq!(status["queue_duration_ms"], 627_000);
        assert_eq!(status["queue_votes"], 5);
        assert_eq!(status["history_entries"], 1);
        assert_eq!(status["next_track"]["title"], "Blue");
        assert_eq!(status["services"]["mopidy_online"], true);
        assert_eq!(status["display"]["carousel_cover_count"], 13);
        assert_eq!(
            status["system"]["orpheus_version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(status["timestamp_ms"].as_u64().unwrap() > 1_700_000_000_000);
        assert!(!body.contains("private-cover"));
        assert!(!body.contains("private-current"));
        assert!(!body.contains("Private requester"));
        assert!(!body.contains("private-history"));
    }

    #[test]
    fn playback_and_configuration_are_separate_pages() {
        let playback = Arc::new(Mutex::new(WebPlaybackStatus {
            online: true,
            mopidy_online: true,
            title: "Player only".to_string(),
            ..WebPlaybackStatus::default()
        }));
        let player = http_get("/", Arc::clone(&playback));
        let settings = http_get("/settings", playback);
        let (_, player_body) = response_parts(&player);
        let (_, settings_body) = response_parts(&settings);

        assert!(player_body.contains("aria-current=\"page\">Playback"));
        assert!(player_body.contains("id=\"playback-form\""));
        assert!(player_body.contains("id=\"quick-playlist-form\""));
        assert!(!player_body.contains("id=\"settings-form\""));
        assert!(!player_body.contains("id=\"install-app\""));
        assert!(settings_body.contains("aria-current=\"page\">Configuration"));
        assert!(settings_body.contains("id=\"settings-form\""));
        assert!(settings_body.contains("id=\"install-app\""));
        assert!(!settings_body.contains("id=\"playback-form\""));
        assert!(settings_body.contains("src=\"/settings.js\""));
        assert!(settings_body.contains("src=\"/pwa.js\""));
    }

    #[test]
    fn pwa_manifest_worker_and_icons_are_well_formed() {
        let playback = Arc::new(Mutex::new(WebPlaybackStatus::default()));
        let manifest = http_get("/manifest.webmanifest", Arc::clone(&playback));
        let (manifest_headers, manifest_body) = response_parts(&manifest);
        let manifest_json: Value = serde_json::from_str(manifest_body).unwrap();
        assert!(manifest_headers.contains("Content-Type: application/manifest+json"));
        assert_eq!(manifest_json["display"], "standalone");
        assert_eq!(manifest_json["start_url"], "/");
        assert_eq!(manifest_json["icons"][0]["sizes"], "192x192");
        assert_eq!(manifest_json["icons"][1]["sizes"], "512x512");
        assert_eq!(manifest_json["shortcuts"][1]["url"], "/history");
        assert_eq!(manifest_json["shortcuts"][2]["url"], "/diagnostics");
        assert_eq!(manifest_json["shortcuts"][3]["url"], "/settings");

        let worker = http_get("/service-worker.js", Arc::clone(&playback));
        let (worker_headers, worker_body) = response_parts(&worker);
        assert!(worker_headers.contains("Service-Worker-Allowed: /"));
        assert!(worker_body.contains("orpheus-shell-v9"));
        assert!(worker_body.contains("'/app.js?v=9'"));
        assert!(worker_body.contains("'/identity.js?v=1'"));
        assert!(worker_body.contains("event.request.mode === 'navigate'"));
        assert!(worker_body.contains("url.pathname.startsWith('/api/')"));
        assert!(worker_body.contains("fetch(event.request)"));
        assert!(worker_body.contains("cache.put(event.request, copy)"));

        let install = http_get("/pwa.js", Arc::clone(&playback));
        let (_, install_body) = response_parts(&install);
        assert!(install_body.contains("beforeinstallprompt"));
        assert!(install_body.contains("window.isSecureContext"));
        assert!(install_body.contains("Add to Home Screen"));

        let identity = http_get("/identity.js", Arc::clone(&playback));
        let (identity_headers, identity_body) = response_parts(&identity);
        assert!(identity_headers.contains("Content-Type: text/javascript; charset=utf-8"));
        assert!(identity_body.contains("orpheus-requester-name"));
        assert!(identity_body.contains("main.setAttribute('inert', '')"));
        assert!(identity_body.contains("window.localStorage.setItem(storageKey, name)"));

        let settings_script = http_get("/settings.js", Arc::clone(&playback));
        let (_, settings_script_body) = response_parts(&settings_script);
        assert!(settings_script_body.contains("visualizerDelay.addEventListener('input'"));
        assert!(settings_script_body.contains("'/visualizer-delay'"));
        assert!(settings_script_body.contains("'visualizer_delay_ms'"));
        assert!(settings_script_body.contains("spotifyVisualizerDelay.addEventListener('input'"));
        assert!(settings_script_body.contains("'/spotify-visualizer-delay'"));
        assert!(settings_script_body.contains("'spotify_visualizer_extra_delay_ms'"));
        assert!(settings_script_body.contains("carouselSpeed.addEventListener('input'"));
        assert!(settings_script_body.contains("'/carousel-speed'"));
        assert!(settings_script_body.contains("'carousel_speed'"));

        for (path, expected_size) in [("/icon-192.png", 192), ("/icon-512.png", 512)] {
            let response = http_request_bytes(
                &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
                Arc::clone(&playback),
            );
            let (headers, body) = byte_response_parts(&response);
            assert!(headers.contains("Content-Type: image/png"));
            let icon = image::load_from_memory(body).unwrap();
            assert_eq!(icon.width(), expected_size);
            assert_eq!(icon.height(), expected_size);
        }
    }

    #[test]
    fn live_script_is_same_origin_only_and_uses_safe_dom_updates() {
        let response = http_get(
            "/app.js",
            Arc::new(Mutex::new(WebPlaybackStatus::default())),
        );
        let (headers, body) = response_parts(&response);

        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(headers.contains("Content-Type: text/javascript; charset=utf-8"));
        assert!(headers.contains("script-src 'self'"));
        assert!(headers.contains("worker-src 'self'"));
        assert!(headers.contains("manifest-src 'self'"));
        assert!(headers.contains("img-src 'self'"));
        assert!(headers.contains("X-Content-Type-Options: nosniff"));
        assert_eq!(body, APP_JS);
        assert!(body.contains("fetch('/api/status'"));
        assert!(body.contains("fetch('/api/playlists'"));
        assert!(body.contains("fetch(`/api/search?q=${encodeURIComponent(query)}`"));
        assert!(
            body.contains(
                "fetch(`/api/browse?kind=${encodeURIComponent(libraryBrowseKind.value)}`"
            )
        );
        assert!(body.contains("fetch(`/api/lookup?uri=${encodeURIComponent(uri)}`"));
        assert!(body.contains("fetch('/api/queue'"));
        assert!(body.contains("fetch('/queue'"));
        assert!(body.contains("fetch('/queue/edit'"));
        assert!(body.contains("fetch('/queue/vote'"));
        assert!(body.contains("new EventSource('/api/events')"));
        assert!(body.contains("get('change-client-name')"));
        assert!(body.contains("Added by ${track.requested_by}"));
        assert!(body.contains("window.setInterval(refreshQueue, 10000)"));
        assert!(body.contains("window.setTimeout(searchLibrary, 350)"));
        assert!(body.contains("librarySearchResults.replaceChildren()"));
        assert!(body.contains("window.setInterval(refresh, 10000)"));
        assert!(body.contains("window.setInterval(refreshPlaylists, 60000)"));
        assert!(body.contains("event.preventDefault()"));
        assert!(body.contains(
            "const playbackEndpoint = playbackForm.getAttribute('action') || '/playback'"
        ));
        assert!(body.contains("fetch(playbackEndpoint"));
        assert!(!body.contains("fetch(playbackForm.action"));
        assert!(body.contains("sendAction('seek'"));
        assert!(body.contains("let pendingVolume = null"));
        assert!(body.contains("let volumeDragging = false"));
        assert!(body.contains("Date.now() >= volumeEditingUntil"));
        assert!(body.contains("if (action !== 'volume') window.setTimeout(refresh, 150)"));
        assert!(body.contains("volume.addEventListener('pointerdown'"));
        assert!(body.contains("volume.addEventListener('pointerup'"));
        assert!(body.contains(
            "volume.addEventListener('input', () => setWebVolume(Number(volume.value), false))"
        ));
        assert!(body.contains("volume.addEventListener('change'"));
        assert!(body.contains("window.clearTimeout(volumeTimer)"));
        assert!(body.contains("commitWebVolume(target), 120"));
        assert!(body.contains("action !== 'screensaver'"));
        assert!(body.contains("document.addEventListener('keydown'"));
        assert!(body.contains("fetch(quickPlaylistForm.action"));
        assert!(body.contains("playlistSelect.replaceChildren()"));
        assert!(body.contains("textContent"));
        assert!(!body.contains("innerHTML"));
    }

    #[test]
    fn ajax_playback_request_returns_json_without_a_page_redirect() {
        let body = "csrf=test-csrf-token&action=seek&position_seconds=45";
        let request = format!(
            "POST /playback HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let response = http_request(&request, Arc::new(Mutex::new(WebPlaybackStatus::default())));
        let (headers, body) = response_parts(&response);

        assert!(headers.starts_with("HTTP/1.1 202 Accepted\r\n"));
        assert!(!headers.contains("Location:"));
        assert_eq!(body, "{\"ok\":true}");
    }

    #[test]
    fn live_visualizer_delay_update_changes_only_the_offset() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "orpheus-web-settings-{}-{unique}.toml",
            std::process::id()
        ));
        let original = Settings {
            display_brightness: 0.7,
            weather_location: "Keep me".to_string(),
            ..Settings::default()
        };
        original.save(&path).unwrap();
        let request = HttpRequest {
            method: "POST".to_string(),
            target: "/visualizer-delay".to_string(),
            headers: HashMap::from([(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )]),
            body: b"csrf=token&visualizer_delay_ms=850".to_vec(),
        };
        let (updates, received) = mpsc::channel();

        save_visualizer_delay_request(&request, &path, "token", &updates).unwrap();
        let saved = Settings::load(&path.to_string_lossy());
        std::fs::remove_file(path).unwrap();

        assert_eq!(saved.visualizer_delay_ms, 850);
        assert_eq!(saved.display_brightness, 0.7);
        assert_eq!(saved.weather_location, "Keep me");
        let WebConfigUpdate::Settings(applied) = received.recv().unwrap() else {
            panic!("expected a settings update");
        };
        assert_eq!(applied.visualizer_delay_ms, 850);
    }

    #[test]
    fn live_spotify_visualizer_delay_updates_only_spotify_offset() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "orpheus-web-spotify-delay-{}-{unique}.toml",
            std::process::id()
        ));
        let original = Settings {
            visualizer_delay_ms: 1_250,
            weather_location: "Keep me".to_string(),
            ..Settings::default()
        };
        original.save(&path).unwrap();
        let request = HttpRequest {
            method: "POST".to_string(),
            target: "/spotify-visualizer-delay".to_string(),
            headers: HashMap::from([(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )]),
            body: b"csrf=token&spotify_visualizer_extra_delay_ms=300".to_vec(),
        };
        let (updates, received) = mpsc::channel();

        save_spotify_visualizer_delay_request(&request, &path, "token", &updates).unwrap();
        let saved = Settings::load(&path.to_string_lossy());
        std::fs::remove_file(path).unwrap();

        assert_eq!(saved.visualizer_delay_ms, 1_250);
        assert_eq!(saved.spotify_visualizer_extra_delay_ms, 300);
        assert_eq!(saved.weather_location, "Keep me");
        let WebConfigUpdate::Settings(applied) = received.recv().unwrap() else {
            panic!("expected a settings update");
        };
        assert_eq!(applied.spotify_visualizer_extra_delay_ms, 300);
    }

    #[test]
    fn live_carousel_speed_update_preserves_other_settings() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "orpheus-web-carousel-{}-{unique}.toml",
            std::process::id()
        ));
        let original = Settings {
            display_brightness: 0.7,
            visualizer_delay_ms: 1_250,
            ..Settings::default()
        };
        original.save(&path).unwrap();
        let request = HttpRequest {
            method: "POST".to_string(),
            target: "/carousel-speed".to_string(),
            headers: HashMap::from([(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )]),
            body: b"csrf=token&carousel_speed=45".to_vec(),
        };
        let (updates, received) = mpsc::channel();

        save_carousel_speed_request(&request, &path, "token", &updates).unwrap();
        let saved = Settings::load(&path.to_string_lossy());
        std::fs::remove_file(path).unwrap();

        assert_eq!(saved.carousel_speed, 45.0);
        assert_eq!(saved.display_brightness, 0.7);
        assert_eq!(saved.visualizer_delay_ms, 1_250);
        let WebConfigUpdate::Settings(applied) = received.recv().unwrap() else {
            panic!("expected a settings update");
        };
        assert_eq!(applied.carousel_speed, 45.0);
    }

    #[test]
    fn artwork_endpoint_serves_detected_image_bytes_without_exposing_its_path() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "orpheus-web-art-{}-{unique}.cache",
            std::process::id()
        ));
        let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46];
        std::fs::write(&path, jpeg).unwrap();
        let playback = WebPlaybackStatus {
            artwork_id: Some("test-art".to_string()),
            artwork_path: Some(path.to_string_lossy().to_string()),
            ..WebPlaybackStatus::default()
        };

        let response = http_request_bytes(
            "GET /api/artwork?v=test-art HTTP/1.1\r\nHost: localhost\r\n\r\n",
            Arc::new(Mutex::new(playback)),
        );
        std::fs::remove_file(path).unwrap();
        let (headers, body) = byte_response_parts(&response);

        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(headers.contains("Content-Type: image/jpeg"));
        assert_eq!(body, jpeg);
    }

    #[test]
    fn artwork_format_detection_uses_content_instead_of_the_cache_extension() {
        assert_eq!(
            image_content_type(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(image_content_type(b"GIF89arest"), Some("image/gif"));
        assert_eq!(
            image_content_type(b"RIFF\x04\x00\x00\x00WEBPrest"),
            Some("image/webp")
        );
        assert_eq!(image_content_type(b"not an image"), None);
    }

    #[test]
    fn playlist_play_route_uses_the_saved_entry_and_returns_without_redirecting() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "orpheus-web-playlists-{}-{unique}.toml",
            std::process::id()
        ));
        let config = Config {
            playlists: vec![
                PlaylistEntry {
                    name: "First".to_string(),
                    uri: "test:playlist:first".to_string(),
                    art_uri: None,
                },
                PlaylistEntry {
                    name: "Saved second".to_string(),
                    uri: "test:playlist:saved-second".to_string(),
                    art_uri: None,
                },
            ],
        };
        config.save(&path).unwrap();
        let body = "csrf=test-csrf-token&playlist_index=1&playlist_uri_1=test%3Aplaylist%3Aposted-tampering";
        let raw_request = format!(
            "POST /playlists/play HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut request = read_request(&mut Cursor::new(raw_request.as_bytes())).unwrap();
        request.headers.insert(
            "cookie".to_string(),
            "orpheus_settings=test-session".to_string(),
        );
        let (updates, received) = mpsc::channel();
        let mut response = Vec::new();
        let server = WebServerState {
            settings_path: PathBuf::from("/unused/settings.toml"),
            playlists_path: path.clone(),
            csrf_token: Arc::new("test-csrf-token".to_string()),
            updates,
            playback_status: Arc::new(Mutex::new(WebPlaybackStatus::default())),
            playlist_catalog: Arc::new(Mutex::new(Vec::new())),
            library_search: test_library_search(),
            library_browse: test_library_browse(),
            library_lookup: test_library_lookup(),
            recent_search_uris: Arc::new(Mutex::new(VecDeque::new())),
            settings_sessions: Arc::new(Mutex::new(HashMap::from([(
                "test-session".to_string(),
                Instant::now(),
            )]))),
            failed_logins: Arc::new(Mutex::new(VecDeque::new())),
            guest_queue_attempts: Arc::new(Mutex::new(HashMap::new())),
            queue_reservations: Arc::new(Mutex::new(HashMap::new())),
            queue_voters: Arc::new(Mutex::new(HashMap::new())),
            queue: Arc::new(Mutex::new(Vec::new())),
            history: Arc::new(Mutex::new(Vec::new())),
            active_clients: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
        };
        dispatch_request(&mut response, request, &server).unwrap();
        std::fs::remove_file(path).unwrap();

        let response = String::from_utf8(response).unwrap();
        let (headers, response_body) = response_parts(&response);
        assert!(headers.starts_with("HTTP/1.1 202 Accepted\r\n"));
        assert!(!headers.contains("Location:"));
        assert_eq!(response_body, "{\"ok\":true}");
        let WebConfigUpdate::PlayPlaylist(entry) = received.recv().unwrap() else {
            panic!("expected a saved playlist play update");
        };
        assert_eq!(entry.name, "Saved second");
        assert_eq!(entry.uri, "test:playlist:saved-second");
    }

    #[test]
    fn configuration_page_edits_favourites_without_playback_controls() {
        let config = Config {
            playlists: vec![PlaylistEntry {
                name: "Rock & Roll".to_string(),
                uri: "test:playlist:rock".to_string(),
                art_uri: None,
            }],
        };
        let html = render_settings_page(&Settings::default(), &config, "token", None);

        assert!(html.contains("value=\"Rock &amp; Roll\""));
        assert!(!html.contains("data-playlist-play"));
        assert!(!html.contains("id=\"playback-form\""));
        assert_eq!(html.matches("<form").count(), 4);
        assert_eq!(html.matches("</form>").count(), 4);
    }

    #[test]
    fn unknown_web_route_returns_a_non_cacheable_404() {
        let response = http_get(
            "/does-not-exist",
            Arc::new(Mutex::new(WebPlaybackStatus::default())),
        );
        let (headers, body) = response_parts(&response);

        assert!(headers.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(headers.contains("Cache-Control: no-store"));
        assert_eq!(body, "Not found\n");
    }

    #[test]
    fn offline_card_disables_audio_controls_but_keeps_display_control_available() {
        let html = playback_card(&WebPlaybackStatus::default(), "token", true);

        assert!(html.contains("Mopidy unavailable"));
        assert_eq!(html.matches("data-playback-control").count(), 5);
        assert_eq!(html.matches(" disabled").count(), 5);
        let screen_button = html
            .split("id=\"toggle-screen\"")
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        assert!(!screen_button.contains("disabled"));
    }

    #[test]
    fn web_time_formatting_handles_boundaries_and_invalid_values() {
        assert_eq!(format_web_time(-1.0), "0:00");
        assert_eq!(format_web_time(59.9), "0:59");
        assert_eq!(format_web_time(60.0), "1:00");
        assert_eq!(format_web_time(3_661.9), "1:01:01");
        assert_eq!(format_web_time(f64::NAN), "0:00");
        assert_eq!(format_web_time(f64::INFINITY), "0:00");
    }
}
