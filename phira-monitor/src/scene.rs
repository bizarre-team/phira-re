use crate::{cloud::download, dir, launch::launch_task, Config};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use log::{debug, error, info, warn};
use macroquad::prelude::*;
use phira_mp_client::Client;
use phira_mp_common::{JudgeEvent, Message, RoomId, RoomState, TouchFrame, UserInfo};
use prpr::{
    core::{BadNote, Chart, ParticleEmitter, Resource, Tweenable, Vector},
    ext::{poll_future, semi_white, LocalTask, RectExt},
    info::ChartInfo,
    judge::{Judge, JudgeStatus},
    scene::{show_error, GameScene, Scene},
    task::Task,
    time::TimeManager,
    ui::Ui,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    path::Path,
    sync::Arc,
};
use tokio::net::TcpStream;

/// Helper to resolve borrow checker issues when we need to call methods on self
/// while client reference is held. This clones the minimal data needed.
macro_rules! with_client {
    ($self:ident, $client:ident, $body:block) => {
        if let Some($client) = &$self.client {
            $body
        }
    };
}

const ASPECT_MIN: f32 = 3. / 2.;
const ASPECT_MAX: f32 = 9. / 5.;

/// Buffer time (in seconds) ahead of the current playback time.
/// If the latest judge event is within this buffer, the player is considered
/// to have enough data and will not pause.
const JUDGE_BUFFER: f64 = 0.5;

/// Resume buffer time: after receiving enough data, wait this duration before actually resuming playback.
/// This gives players visual feedback and prevents abrupt jumps.
const RESUME_BUFFER_TIME: f64 = 1.5;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartEntity {
    pub id: i32,
    pub name: String,
    pub file: String,
    pub chart_updated: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub created: DateTime<Utc>,
    pub uploader: i32,
}

/// Game state for the monitor's state machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MonitorGameState {
    /// Idle / selecting chart
    Idle,
    /// Waiting for all players to receive their first judge
    WaitingReady,
    /// Game is actively playing
    Playing,
    /// Game has ended
    Ended,
}

async fn fetch_chart(id: i32) -> Result<ChartEntity> {
    Ok(reqwest::get(format!("https://phira.5wyxi.com/chart/{id}"))
        .await?
        .error_for_status()?
        .json()
        .await?)
}

pub struct PlayerView {
    id: i32,
    name: String,
    chart: Chart,
    judge: Judge,
    emitter: ParticleEmitter,
    last_update_time: f64,
    touch_points: Vec<(f32, f32)>,
    bad_notes: Vec<BadNote>,

    touches: VecDeque<TouchFrame>,
    judges: VecDeque<JudgeEvent>,

    current_touches: HashMap<i8, Vec2>,
    current_time: f64,

    latest_time: Option<f32>,

    // New fields for independent time management and pause state
    /// Whether this player has received the first judge event (ready to start)
    has_started: bool,
    /// The player's local playback time (starts from 0.0)
    local_time: f64,
    /// Whether the player is paused due to lack of judge data
    is_paused: bool,
    /// Total duration the player has been paused (for end-time calculation)
    total_paused_duration: f64,
    /// When the current pause started (global reference time)
    pause_start_time: Option<f64>,
    /// The time of the last processed judge event
    last_judge_time: f64,

    // Resume buffer fields
    /// When the resume buffer started (global reference time)
    resume_buffer_start: Option<f64>,
    /// Whether the player is currently in resume buffer state
    is_resuming: bool,

    // End detection fields
    /// Number of notes that have been judged so far
    judged_notes_count: u32,

    // Hold note tracking fields
    /// Number of notes currently in Hold state (actively being held)
    active_hold_notes: u32,
    /// Time of the last judge event that was processed
    last_processed_judge_time: f64,
    /// Whether we've ever received judges (to handle initial empty state)
    has_received_judges: bool,
    /// Expected end time of the chart (last note's end time)
    expected_end_time: f64,
    /// Whether this player has aborted the game
    is_aborted: bool,

    // FC/AP state for judge line color
    /// Current judge line color state: 0=perfect(gold), 1=good(blue), 2=white
    fc_ap_state: u8,
}

impl PlayerView {
    pub fn new(info: UserInfo, chart: Chart, emitter: ParticleEmitter) -> Self {
        let judge = Judge::new(&chart);
        // Calculate expected end time from the last note's end time
        let expected_end_time = chart.lines.iter().flat_map(|line| {
            line.notes.iter().filter(|n| !n.fake).map(|note| {
                match &note.kind {
                    prpr::core::NoteKind::Hold { end_time, .. } => *end_time,
                    _ => note.time,
                }
            })
        }).fold(0.0, f64::max);
        Self {
            id: info.id,
            name: info.name,
            chart,
            judge,
            emitter,
            last_update_time: 0.,
            touch_points: Vec::new(),
            bad_notes: Vec::new(),

            touches: VecDeque::new(),
            judges: VecDeque::new(),

            current_touches: HashMap::new(),
            current_time: 0.,

            latest_time: None,

            has_started: false,
            local_time: 0.0,
            is_paused: false,
            total_paused_duration: 0.0,
            pause_start_time: None,
            last_judge_time: 0.0,

            resume_buffer_start: None,
            is_resuming: false,

            judged_notes_count: 0,

            active_hold_notes: 0,
            last_processed_judge_time: 0.0,
            has_received_judges: false,
            expected_end_time,
            is_aborted: false,
            fc_ap_state: 0,
        }
    }

    pub fn update(&mut self, client: &Client) {
        // Ignore all events from aborted players
        if self.is_aborted {
            return;
        }

        let player = client.live_player(self.id);

        let mut guard = player.touch_frames.blocking_lock();
        if !guard.is_empty() {
            debug!("received {} touch frames from {}", guard.len(), self.id);
        }
        self.touches.extend(guard.drain(..));
        drop(guard);

        if let Some(back) = self.touches.back() {
            self.latest_time = Some(back.time);
        }

        let mut guard = player.judge_events.blocking_lock();
        if !guard.is_empty() {
            debug!("received {} judge events from {}", guard.len(), self.id);
        }
        // Check if this is the first judge event received
        let had_judges = !self.judges.is_empty();
        self.judges.extend(guard.drain(..));
        if !self.judges.is_empty() {
            self.has_received_judges = true;
        }
        if !had_judges && !self.judges.is_empty() {
            self.has_started = true;
            info!("Player {} has received first judge event", self.name);
        }
        drop(guard);
    }

    /// Update the active hold note count based on current chart state.
    fn update_active_hold_notes(&mut self) {
        self.active_hold_notes = self.chart.lines.iter().map(|line| {
            line.notes.iter().filter(|note| {
                matches!(note.judge, JudgeStatus::Hold(_, _, _, _, _))
            }).count() as u32
        }).sum();
    }

    /// Check if the player has enough judge data to continue playing.
    /// Returns true if the player should pause.
    ///
    /// Core logic: We look at the chart state directly. If the current playback time
    /// has reached a note that should have been judged (based on chart state), but
    /// the chart shows it's still NotJudged, then the judge is truly missing.
    /// This correctly handles:
    /// - Long gaps between notes (no false pause during the gap)
    /// - Hold notes (we only need a judge at the start time)
    /// - Near the end of the chart
    fn should_pause(&self) -> bool {
        // Never pause if game is essentially over (within last 2 seconds)
        if self.local_time >= self.expected_end_time - 2.0 {
            return false;
        }

        // Check chart state directly: find notes that should have been judged by now
        // but are still NotJudged. This means their judge is truly missing.
        let mut has_missing_judge = false;
        let mut earliest_missing_time: Option<f64> = None;

        for line in &self.chart.lines {
            for note in &line.notes {
                if note.fake {
                    continue;
                }
                // A note needs a judge if:
                // 1. It's still NotJudged (not processed yet)
                // 2. Its time has passed (or is very close)
                if matches!(note.judge, JudgeStatus::NotJudged) {
                    let note_time = note.time;
                    // Note needs judge when we're past its time + small buffer
                    if self.local_time > note_time + JUDGE_BUFFER {
                        has_missing_judge = true;
                        if earliest_missing_time.is_none() || note_time < earliest_missing_time.unwrap() {
                            earliest_missing_time = Some(note_time);
                        }
                    }
                }
            }
        }

        // If no notes are missing judges, don't pause
        // This handles:
        // - All notes up to current time have been judged
        // - Next note is in the future (long gap, hold note duration, etc.)
        // - Hold notes (they're in Hold state, not NotJudged)
        if !has_missing_judge {
            return false;
        }

        // We have a missing judge. Check if it's truly missing or just delayed.
        // If judges queue has events for future notes, the missing one might arrive soon.
        if let Some(missing_time) = earliest_missing_time {
            // Check if we have any judges that could be for this note
            // (judges might arrive out of order or with delay)
            if let Some(first_judge) = self.judges.front() {
                let judge_time = first_judge.time as f64;
                // If the earliest pending judge is for a note AFTER the missing one,
                // and the missing one is significantly past due, then it's truly missing
                if judge_time > missing_time + JUDGE_BUFFER {
                    return true;
                }
                // Otherwise, the judge might be arriving soon (out of order or delayed)
                return false;
            }

            // No judges in queue at all - check if we've been waiting too long
            if self.has_received_judges && self.last_processed_judge_time > 0.0 {
                let time_since_last_judge = self.local_time - self.last_processed_judge_time;
                let time_past_missing = self.local_time - missing_time;

                // Pause if:
                // 1. We're past the missing note time by more than buffer, AND
                // 2. We've been without any judges for a significant time
                return time_past_missing > JUDGE_BUFFER && time_since_last_judge > JUDGE_BUFFER;
            }
        }

        false
    }

    /// Update the player's local time based on the global delta.
    /// Handles pause/resume logic with resume buffer based on judge availability.
    pub fn update_time(&mut self, global_delta: f64, global_now: f64) {
        if !self.has_started || self.is_aborted {
            return;
        }

        let should_pause = self.should_pause();

        if should_pause && !self.is_paused && !self.is_resuming {
            // Enter pause state
            self.is_paused = true;
            self.pause_start_time = Some(global_now);
            self.resume_buffer_start = None;
            self.is_resuming = false;
            debug!("Player {} paused at local_time={:.2}", self.name, self.local_time);
        } else if !should_pause && self.is_paused {
            // Start resume buffer (not immediately resuming)
            self.is_paused = false;
            self.is_resuming = true;
            self.resume_buffer_start = Some(global_now);
            if let Some(start) = self.pause_start_time {
                self.total_paused_duration += global_now - start;
                self.pause_start_time = None;
            }
            debug!("Player {} entering resume buffer at local_time={:.2}", self.name, self.local_time);
        }

        // Check if resume buffer is complete
        if self.is_resuming {
            if let Some(buffer_start) = self.resume_buffer_start {
                if global_now >= buffer_start + RESUME_BUFFER_TIME {
                    // Buffer complete, truly resume
                    self.is_resuming = false;
                    self.resume_buffer_start = None;
                    debug!("Player {} resume buffer complete at local_time={:.2}", self.name, self.local_time);
                }
            }
        }

        // Only advance time when not paused and not in resume buffer
        if !self.is_paused && !self.is_resuming {
            self.local_time += global_delta;
        }
    }



    fn update_with_res_at_time(&mut self, res: &mut Resource, t: f64) {
        let mut updated = false;
        while self.touches.front().is_some_and(|it| t > it.time as f64) {
            let Some(frame) = self.touches.pop_front() else { unreachable!() };
            for (id, pos) in frame.points {
                if id >= 0 {
                    self.current_touches.insert(id, Vec2::new(pos.x(), pos.y()));
                } else {
                    self.current_touches.remove(&!id);
                }
            }
            self.current_time = frame.time as f64;
            updated = true;
        }
        if updated {
            self.touch_points.clear();
            if let Some(frame) = self.touches.front() {
                let mut current = self.current_touches.clone();
                self.touch_points.extend(frame.points.iter().filter_map(|(id, pos)| {
                    let pos = vec2(pos.x(), pos.y());
                    let id = if *id >= 0 { *id } else { !*id };
                    let pos = if let Some(old) = current.remove(&id) {
                        Vec2::tween(&old, &pos, ((t - self.current_time) / (frame.time as f64 - self.current_time)) as f32)
                    } else {
                        return None;
                    };
                    Some((pos.x, pos.y / res.aspect_ratio))
                }));
                self.touch_points.extend(current.into_values().map(|it| (it.x, it.y)));
            }
        }

        std::mem::swap(&mut self.emitter, &mut res.emitter);
        self.chart.update(res);

        while let Some(event) = self.judges.front() {
            if event.time as f64 > t {
                break;
            }
            let Some(event) = self.judges.pop_front() else { unreachable!() };
            self.last_judge_time = event.time as f64;
            self.last_processed_judge_time = event.time as f64;
            use phira_mp_common::Judgement::*;
            use prpr::judge::Judgement as TJ;
            let kind = match event.judgement {
                Perfect => Ok(TJ::Perfect),
                Good => Ok(TJ::Good),
                Bad => Ok(TJ::Bad),
                Miss => Ok(TJ::Miss),
                HoldPerfect => Err(true),
                HoldGood => Err(false),
            };
            let note = &mut self.chart.lines[event.line_id as usize].notes[event.note_id as usize];

            match kind {
                Ok(tj) => {
                    note.judge = JudgeStatus::Judged;
                    self.judged_notes_count += 1;
                    let line = &self.chart.lines[event.line_id as usize];
                    let line_tr = line.now_transform(res, &self.chart.lines);
                    let note = &line.notes[event.note_id as usize];
                    self.judge.commit(t, tj, event.line_id, event.note_id, 0.);
                    match tj {
                        TJ::Perfect => {
                            res.with_model(line_tr * note.object.now(res), |res| {
                                res.emit_at_origin(note.rotation(line), res.res_pack.info.fx_perfect())
                            });
                        }
                        TJ::Good => {
                            res.with_model(line_tr * note.object.now(res), |res| {
                                res.emit_at_origin(note.rotation(line), res.res_pack.info.fx_good())
                            });
                        }
                        TJ::Bad => {
                            self.bad_notes.push(BadNote {
                                time: t,
                                kind: note.kind.clone(),
                                matrix: {
                                    let mut mat = line_tr;
                                    if !note.above {
                                        mat.append_nonuniform_scaling_mut(&Vector::new(1., -1.));
                                    }
                                    let incline_sin = line.incline.now_opt().map(|it| it.to_radians().sin()).unwrap_or_default();
                                    mat *= note.now_transform(
                                        res,
                                        &line.ctrl_obj.borrow_mut(),
                                        ((note.height - line.height.now() as f64) / res.aspect_ratio as f64 * note.speed) as f32,
                                        incline_sin,
                                    );
                                    mat
                                },
                            });
                        }
                        _ => {}
                    }
                }
                Err(perfect) => {
                    note.judge = JudgeStatus::Hold(perfect, t, 0., false, f64::INFINITY);
                    self.active_hold_notes += 1;
                }
            }
        }

        // Update active hold note count (some may have ended)
        self.update_active_hold_notes();

        // Update FC/AP state based on judge counts
        // State machine: perfect(0) -> good(1) -> white(2), no return
        let counts = self.judge.counts();
        if self.fc_ap_state == 0 && counts[1] > 0 {
            self.fc_ap_state = 1; // good
        }
        if self.fc_ap_state <= 1 && (counts[2] > 0 || counts[3] > 0) {
            self.fc_ap_state = 2; // white
        }

        std::mem::swap(&mut self.emitter, &mut res.emitter);
    }

    fn swap(&mut self, scene: &mut GameScene) {
        use std::mem::swap;
        swap(&mut self.chart, &mut scene.chart);
        swap(&mut self.judge, &mut scene.judge);
        swap(&mut self.emitter, &mut scene.res.emitter);
        swap(&mut self.last_update_time, &mut scene.last_update_time);
        swap(&mut self.touch_points, &mut scene.touch_points);
        swap(&mut self.bad_notes, &mut scene.bad_notes);
    }

    pub fn render(&mut self, ui: &mut Ui, r: Rect, game_scene: Option<&mut GameScene>, global_now: f64, game_state: MonitorGameState) -> Result<()> {
        if let Some(scene) = game_scene {
            // Set the resource time to this player's local time
            scene.res.time = self.local_time;
            self.update_with_res_at_time(&mut scene.res, self.local_time);

            // Apply per-player FC/AP judge line color
            // Only when the chart line does not have a custom color animation
            // (chart custom color takes precedence via unwrap_or in line.rs)
            scene.res.judge_line_color = match self.fc_ap_state {
                0 => scene.res.res_pack.info.color_perfect(),
                1 => scene.res.res_pack.info.color_good(),
                _ => Color::new(1.0, 1.0, 1.0, 1.0),
            };

            let r = ui.rect_to_global(r);
            let vw = screen_width();
            let x = (r.x + 1.) / 2. * vw;
            let y = (r.y + ui.top) / 2. * vw;
            let w = r.w * vw / 2.;
            let h = r.h * vw / 2.;
            let mut ui = Ui::new(ui.text_painter, Some((x as _, (screen_height() - y - h) as _, w as _, h as _)));

            push_camera_state();
            self.swap(scene);
            // Create a temporary TimeManager that reports the player's local time
            let player_time = self.local_time;
            let mut player_tm = TimeManager::manual(Box::new(move || player_time));
            scene.render(&mut player_tm, &mut ui)?;
            self.swap(scene);
            pop_camera_state();

            unsafe { get_internal_gl() }.quad_gl.viewport(None);
        }

        // Draw player name (always visible)
        ui.text(&self.name)
            .pos(r.right() - 0.013, r.bottom() - 0.016)
            .anchor(1., 1.)
            .size(0.7)
            .draw();

        // State-machine driven UI overlay
        match game_state {
            MonitorGameState::Playing | MonitorGameState::WaitingReady => {
                if self.is_aborted {
                    self.draw_abort_indicator(ui, r);
                } else {
                    self.draw_pause_or_resume_indicator(ui, r, global_now);
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Draw abort indicator overlay with blue styling.
    /// No time information is displayed.
    fn draw_abort_indicator(&self, ui: &mut Ui, r: Rect) {
        // Semi-transparent dark overlay
        ui.fill_rect(r, Color::new(0.0, 0.0, 0.0, 0.7));

        let ct = r.center();
        ui.text("Aborted")
            .pos(ct.x, ct.y)
            .anchor(0.5, 0.5)
            .size(0.8)
            .color(Color::new(0.2, 0.4, 1.0, 1.0))
            .draw();
    }

    /// Draw pause or resume buffer indicator based on player state.
    fn draw_pause_or_resume_indicator(&self, ui: &mut Ui, r: Rect, global_now: f64) {
        if self.is_paused {
            // Pause: semi-transparent overlay + blinking "Paused"
            let alpha = (0.5 + 0.3 * (global_now * 3.0).sin() as f32).clamp(0.3, 0.8);
            ui.fill_rect(r, Color::new(0.0, 0.0, 0.0, alpha * 0.5));
            let ct = r.center();
            ui.text("Paused")
                .pos(ct.x, ct.y)
                .anchor(0.5, 0.5)
                .size(0.8)
                .color(Color::new(1.0, 0.2, 0.2, alpha))
                .draw();
        } else if self.is_resuming {
            // Resume buffer: yellow "Resuming..." + progress bar
            if let Some(buffer_start) = self.resume_buffer_start {
                let elapsed = global_now - buffer_start;
                let progress = ((elapsed / RESUME_BUFFER_TIME) as f32).clamp(0.0, 1.0);

                let ct = r.center();
                ui.text("Resuming...")
                    .pos(ct.x, ct.y - 0.05)
                    .anchor(0.5, 0.5)
                    .size(0.6)
                    .color(Color::new(1.0, 0.9, 0.2, 0.9))
                    .draw();

                // Progress bar background
                let bar_w = 0.3_f32;
                let bar_h = 0.02_f32;
                let bar_x = ct.x - bar_w / 2.;
                let bar_y = ct.y + 0.05;
                ui.fill_rect(Rect::new(bar_x, bar_y, bar_w, bar_h),
                            Color::new(0.3, 0.3, 0.3, 0.8));
                // Progress bar fill
                ui.fill_rect(Rect::new(bar_x, bar_y, bar_w * progress, bar_h),
                            Color::new(1.0, 0.9, 0.2, 0.9));
            }
        }
    }

}

struct InitResult {
    client: Client,
    chart: Option<(i32, String)>,
    token: String,
}

fn create_init_task(config: Config, token: Option<String>) -> Task<Result<InitResult>> {
    Task::new(async move {
        #[derive(Serialize)]
        struct LoginP<'a> {
            email: &'a str,
            password: &'a str,
        }

        let token = if let Some(token) = token {
            token
        } else {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct LoginR {
                token: String,
            }
            info!("登录中…");
            let resp: LoginR = reqwest::Client::new()
                .post("https://api.phira.cn/login")
                .json(&LoginP {
                    email: &config.email,
                    password: &config.password,
                })
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            resp.token
        };

        info!("连接 & 鉴权中…");
        let client = Client::new(TcpStream::connect(&config.server).await.context("连接到服务器失败")?)
            .await
            .context("连接失败")?;
        client.authenticate(token.clone()).await?;

        info!("加入房间…");
        let room_id: RoomId = config.room_id.clone().try_into().context("房间 ID 不合法")?;
        if client.room_state().await.is_none() {
            client.join_room(room_id, true).await?;
        }

        let chart = if let RoomState::SelectChart(Some(id)) = client.room_state().await.unwrap() {
            Some((id, fetch_chart(id).await?.name))
        } else {
            None
        };

        info!("初始化完成");

        Ok(InitResult { client, chart, token })
    })
}

pub struct MainScene {
    config: Config,
    client: Option<Arc<Client>>,

    token: Option<String>,
    init_task: Option<Task<Result<InitResult>>>,
    messages: Vec<String>,

    scene_task: LocalTask<Result<(GameScene, Vec<PlayerView>)>>,

    selected_chart: Option<(i32, String)>,

    get_ready_task: Option<Task<Result<()>>>,

    game_scene: Option<GameScene>,
    /// Global time manager for audio synchronization (independent of players)
    tm: TimeManager,
    /// Whether rendering and audio have started
    render_started: bool,
    /// Whether all players have received their first judge and playback has begun
    all_players_ready: bool,
    /// Global time when playback started
    playback_start_time: Option<f64>,

    players: Vec<PlayerView>,
    start_playing_time: f32,

    scores: HashMap<String, (u32, f32, bool)>,
    game_end: bool,

    /// Last global time for delta calculation
    last_global_time: f64,

    // State machine
    /// Current monitor game state
    monitor_game_state: MonitorGameState,
}

impl MainScene {
    pub async fn new(config: Config) -> Result<Self> {
        Ok(Self {
            config: config.clone(),
            client: None,

            token: None,
            init_task: Some(create_init_task(config, None)),
            messages: Vec::new(),

            scene_task: None,

            selected_chart: None,

            get_ready_task: None,

            game_scene: None,
            tm: TimeManager::default(),
            render_started: false,
            all_players_ready: false,
            playback_start_time: None,

            players: Vec::new(),
            start_playing_time: f32::NAN,

            scores: HashMap::new(),
            game_end: false,

            last_global_time: 0.0,

            monitor_game_state: MonitorGameState::Idle,
        })
    }

    fn transition_state(&mut self, new_state: MonitorGameState) {
        if self.monitor_game_state != new_state {
            info!("Game state transition: {:?} -> {:?}", self.monitor_game_state, new_state);
            self.monitor_game_state = new_state;
        }
    }

    fn start_get_ready(&mut self) {
        let client = self.client.as_ref().map(Arc::clone).unwrap();
        let id = self.selected_chart.as_ref().unwrap().0;
        let token = self.token.clone().unwrap();
        self.render_started = false;
        self.all_players_ready = false;
        self.playback_start_time = None;
        self.game_scene = None;
        self.get_ready_task = Some(Task::new(async move {
            let entity = fetch_chart(id).await?;
            info!("谱面信息：{entity:?}");
            let path = format!("download/{id}");
            let info_path = format!("{}/{path}/info.yml", dir::charts()?);
            let should_download = if Path::new(&info_path).exists() {
                let local_info: ChartInfo = serde_yaml::from_reader(File::open(info_path)?)?;
                local_info
                    .updated
                    .map_or(entity.updated != entity.created, |local_updated| local_updated != entity.updated)
            } else {
                true
            };
            if should_download {
                let local_path = download(entity, token).await?;
                info!("已下载到 {local_path}");
            } else {
                info!("无需下载");
            }

            client.ready().await?;
            Ok(())
        }));
    }

    /// Check if all non-aborted players have received their first judge event
    fn check_all_players_ready(&self) -> bool {
        let active_players: Vec<_> = self.players.iter().filter(|p| !p.is_aborted).collect();
        if active_players.is_empty() {
            return false;
        }
        active_players.iter().all(|p| p.has_started)
    }

    /// Start playback from time=0 for all players and audio
    fn start_playback(&mut self) -> Result<()> {
        info!("All players ready! Starting playback from time=0");
        self.all_players_ready = true;
        self.render_started = true;
        self.tm.speed = 1.;
        self.tm.reset();
        self.playback_start_time = Some(self.tm.real_time());
        self.last_global_time = self.tm.now();

        // Reset all players to time=0
        for player in &mut self.players {
            player.local_time = 0.0;
            player.is_paused = false;
            player.is_resuming = false;
            player.total_paused_duration = 0.0;
            player.pause_start_time = None;
            player.resume_buffer_start = None;
        }

        // Initialize game scene for audio playback
        if let Some(scene) = &mut self.game_scene {
            scene.enter(&mut self.tm, None)?;
        }

        self.transition_state(MonitorGameState::Playing);
        Ok(())
    }

    /// Force end the current game, cleaning up resources and resetting state.
    fn force_end_game(&mut self) {
        if self.monitor_game_state == MonitorGameState::Ended {
            return; // Idempotent
        }
        info!("Force ending game");

        // Pause audio
        if let Some(scene) = &mut self.game_scene {
            let _ = scene.music.pause();
        }

        // Clear any pause/resume states
        for player in &mut self.players {
            player.is_paused = false;
            player.is_resuming = false;
        }

        // Reset playback state
        self.render_started = false;
        self.all_players_ready = false;
        self.playback_start_time = None;

        self.transition_state(MonitorGameState::Ended);
    }

}

impl Scene for MainScene {
    fn touch(&mut self, _tm: &mut TimeManager, _touch: &Touch) -> Result<bool> {
        if self.client.is_none() {
            return Ok(true);
        }
        Ok(false)
    }

    fn update(&mut self, tm: &mut TimeManager) -> Result<()> {
        let t = tm.now() as f32;

        if let Some(task) = &mut self.init_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context("初始化失败"));
                    }
                    Ok(res) => {
                        self.client = Some(Arc::new(res.client));
                        self.selected_chart = res.chart;
                        self.token = Some(res.token);
                    }
                }
            }
        }

        if let Some(task) = &mut self.get_ready_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        warn!("下载谱面失败：{err:?}");
                    }
                    Ok(_) => {
                        self.scene_task = launch_task(
                            self.selected_chart.as_ref().unwrap().0,
                            self.client
                                .as_ref()
                                .unwrap()
                                .blocking_state()
                                .unwrap()
                                .users
                                .values()
                                .filter(|it| !it.monitor)
                                .cloned()
                                .collect(),
                        )?;
                    }
                }
                self.get_ready_task = None;
            }
        }

        if let Some(task) = &mut self.scene_task {
            if let Some(res) = poll_future(task.as_mut()) {
                match res {
                    Err(err) => {
                        error!("failed to load scene: {err:?}");
                    }
                    Ok((scene, players)) => {
                        // Only apply the loaded scene if we're still in WaitingReady or later states.
                        // If StartPlaying was received and cleared game_scene, discard stale results.
                        if self.monitor_game_state != MonitorGameState::Idle {
                            self.game_scene = Some(scene);
                            self.players = players;
                            self.players.sort_by(|x, y| x.name.cmp(&y.name));
                        }
                    }
                }
                self.scene_task = None;
            }
        }

        // Pre-compute values that need client reference
        let room_state = self.client.as_ref().map(|c| c.blocking_room_state().unwrap());
        let is_ready = self.client.as_ref().and_then(|c| c.blocking_is_ready());
        let ping_fail_count = self.client.as_ref().map(|c| c.ping_fail_count()).unwrap_or(0);

        // Collect messages first to avoid borrow issues
        let messages = if let Some(client) = &self.client {
            client.blocking_take_messages()
        } else {
            Vec::new()
        };

        // Pre-compute user names for Chat/Played messages to avoid client borrow later
        let mut chat_messages: Vec<(i32, String)> = Vec::new();
        let mut played_data: Vec<(i32, u32, f32, bool)> = Vec::new();

        for msg in &messages {
            match msg {
                Message::Chat { user, content, .. } => {
                    chat_messages.push((*user, content.clone()));
                }
                Message::SelectChart { id, name, .. } => {
                    self.selected_chart = Some((*id, name.clone()));
                    // If currently playing, force end the game immediately
                    if self.monitor_game_state == MonitorGameState::Playing {
                        info!("SelectChart received while playing, forcing game end");
                        self.force_end_game();
                    }
                    self.transition_state(MonitorGameState::Idle);
                }
                Message::StartPlaying => {
                    self.start_playing_time = t;
                    self.scores.clear();
                    self.game_end = false;
                    // Reset ready state for new round
                    self.all_players_ready = false;
                    self.render_started = false;
                    self.playback_start_time = None;
                    // Note: Do NOT clear game_scene here.
                    // The scene_task may complete after StartPlaying and provide the correct player list.
                    // Clearing game_scene here would prevent the game from starting if scene_task completes later.
                    self.transition_state(MonitorGameState::WaitingReady);

                    // Note: Do NOT sync players with room state here.
                    // The scene_task will provide the correct player list based on
                    // the room state at the time it was started.
                    // Syncing here could remove players who are temporarily not in
                    // the room state (e.g., after abort) but still have game data.

                    for player in &mut self.players {
                        player.has_started = false;
                        player.local_time = 0.0;
                        player.is_paused = false;
                        player.is_resuming = false;
                        player.is_aborted = false;
                        player.total_paused_duration = 0.0;
                        player.pause_start_time = None;
                        player.resume_buffer_start = None;
                        player.judges.clear();
                        player.touches.clear();
                        player.fc_ap_state = 0;
                    }
                }
                Message::Played {
                    user,
                    score,
                    accuracy,
                    full_combo,
                } => {
                    played_data.push((*user, *score as u32, *accuracy, *full_combo));
                }
                Message::GameEnd => {
                    self.game_end = true;
                    // Ensure game end is reliably triggered when playing
                    if self.monitor_game_state == MonitorGameState::Playing {
                        info!("GameEnd message received, ending game");
                        self.force_end_game();
                    }
                }
                Message::Abort { user } => {
                    // Mark player as aborted, but do NOT remove from players list
                    // The player is still in the room, just not playing this round
                    if let Some(player) = self.players.iter_mut().find(|p| p.id == *user) {
                        info!("Player {} (id={}) has aborted", player.name, user);
                        player.is_aborted = true;
                        player.is_paused = true;
                        player.is_resuming = false;
                        player.resume_buffer_start = None;
                    } else {
                        warn!("Abort message received for unknown player id={}", user);
                    }
                }
                _ => {
                    info!("{msg:?}");
                }
            }
        }

        // Process Chat messages after the loop to resolve borrow issues
        for (user_id, content) in chat_messages {
            with_client!(self, client, {
                let user = client.user_name(user_id);
                info!("[{user}] {content}");
                self.messages.push(format!("[{}] [{user}] {content}", Local::now().format("%H:%M:%S")));
            });
        }

        // Process Played messages after the loop to resolve borrow issues
        for (user_id, score, accuracy, full_combo) in played_data {
            with_client!(self, client, {
                let user = client.user_name(user_id);
                info!("{user} played: {score} {accuracy} {full_combo}");
                self.scores.insert(user, (score as _, accuracy, full_combo));
            });
        }

        with_client!(self, client, {
            // Check if all players are ready to start (received first judge)
            let all_ready = self.check_all_players_ready();

            // Receive network data for all non-aborted players
            for player in self.players.iter_mut().filter(|p| !p.is_aborted) {
                player.update(client);
            }

            // Check if all players are ready to start (received first judge)
            if !self.all_players_ready && !self.render_started && self.game_scene.is_some() && all_ready {
                if let Err(err) = self.start_playback() {
                    warn!("Failed to start playback: {err:?}");
                }
            }

            let should_start_ready = self.get_ready_task.is_none()
                && matches!(room_state, Some(RoomState::WaitingForReady))
                && !is_ready.unwrap_or(false);
            if should_start_ready {
                self.start_get_ready();
            }
        });

        if ping_fail_count >= 2 && self.init_task.is_none() {
            warn!("lost connection, re-connecting…");
            self.init_task = Some(create_init_task(self.config.clone(), self.token.clone()));
        }

        // Update global time manager for audio
        if self.render_started && self.all_players_ready {
            let global_now = self.tm.now();
            let global_delta = global_now - self.last_global_time;
            self.last_global_time = global_now;

            // Update each player's local time (with independent pause handling)
            for player in &mut self.players {
                player.update_time(global_delta, global_now);
            }

            // Update game scene (audio) using global time
            if let Some(scene) = &mut self.game_scene {
                scene.update(&mut self.tm)?;
            }


        }

        Ok(())
    }

    fn render(&mut self, tm: &mut TimeManager, ui: &mut Ui) -> Result<()> {
        set_camera(&ui.camera());
        let t = tm.now() as f32;
        let global_now = self.tm.now();

        ui.fill_rect(ui.screen_rect(), ui.background());

        let Some(client) = &self.client else {
            ui.full_loading_simple(t);
            return Ok(());
        };

        let width = 2.;

        let r = Rect::new(-1., -ui.top, width, ui.top * 2.);
        ui.fill_rect(r, semi_white(0.4));

        match client.blocking_room_state().unwrap() {
            RoomState::SelectChart(_) => {
                self.game_scene = None;
                let ct = r.center();
                let tr = ui.text("选曲中").pos(ct.x, ct.y).anchor(0.5, 0.5).no_baseline().size(1.6).draw();
                if let Some((id, name)) = &self.selected_chart {
                    ui.text(format!("{name} (#{id})"))
                        .pos(ct.x, tr.bottom() + 0.03)
                        .anchor(0.5, 0.)
                        .size(0.56)
                        .draw();
                }
                let r = ui.text("上一局成绩").pos(r.x + 0.01, r.y + 0.01).size(0.6).draw();
                ui.scope(|ui| {
                    let s = 0.5;
                    ui.dx(r.x + 0.06);
                    ui.dy(r.bottom() + 0.02);
                    let w = 0.24;
                    for (user, (score, accuracy, full_combo)) in self.scores.iter() {
                        let r = ui.text(user).max_width(w - 0.02).size(s).draw();
                        ui.text(format!("{score} ({:.2}%){}", accuracy * 100., if *full_combo { " 全连" } else { "" }))
                            .pos(w, 0.)
                            .size(s)
                            .draw();
                        ui.dy(r.h + 0.01);
                    }
                });
            }
            _ => {
                // State-machine driven rendering
                match self.monitor_game_state {
                    MonitorGameState::Idle => {
                        // Idle state: show waiting message
                        let ct = r.center();
                        ui.text("等待开始…")
                            .pos(ct.x, ct.y)
                            .anchor(0.5, 0.5)
                            .no_baseline()
                            .size(1.2)
                            .draw();
                    }
                    MonitorGameState::WaitingReady => {
                        // Waiting for all players to receive first judge
                        let ct = r.center();
                        ui.text("等待所有玩家…")
                            .pos(ct.x, ct.y)
                            .anchor(0.5, 0.5)
                            .no_baseline()
                            .size(1.2)
                            .draw();

                        // Show per-player status
                        ui.scope(|ui| {
                            ui.dy(ct.y + 0.1);
                            let s = 0.4;
                            for player in &self.players {
                                let status = if player.has_started { "就绪" } else { "等待中…" };
                                let color = if player.has_started {
                                    Color::new(0.2, 1.0, 0.2, 0.9)
                                } else {
                                    Color::new(1.0, 0.8, 0.2, 0.9)
                                };
                                let r = ui.text(format!("{}: {}", player.name, status))
                                    .anchor(0.5, 0.)
                                    .size(s)
                                    .color(color)
                                    .draw();
                                ui.dy(r.h + 0.02);
                            }
                        });

                    }
                    MonitorGameState::Playing => {
                        // Render all players with their independent local times
                        // Pause/resume indicators are shown when players are paused
                        let (row_count, col_count) = if self.players.len() > 2 {
                            (2, self.players.len().div_ceil(2))
                        } else {
                            (1, self.players.len())
                        };

                        let r = Rect::new(r.x, r.y, r.w / col_count as f32, r.h / row_count as f32);
                        let (w, h) = (r.w.min(r.h * ASPECT_MAX), r.h.min(r.w / ASPECT_MIN));
                        let ct = r.center();
                        let mut iter = self.players.iter_mut();
                        for i in 0..row_count {
                            for j in 0..iter.len().min(col_count) {
                                let r = Rect::new(ct.x + j as f32 * r.w, ct.y + i as f32 * r.h, 0., 0.)
                                    .nonuniform_feather(w / 2., h / 2.)
                                    .feather(-0.01);
                                let player = iter.next().unwrap();
                                player.render(ui, r, self.game_scene.as_mut(), global_now, self.monitor_game_state)?;
                            }
                        }
                    }
                    MonitorGameState::Ended => {
                        // Only render game画面 when room is actually in Playing state
                        // Otherwise show waiting message to prevent stale画面 after game end
                        let current_room_state = client.blocking_room_state().unwrap();
                        if matches!(current_room_state, RoomState::Playing) {
                            // Render all players but force hide pause/resume indicators
                            // Each player shows "Finished" overlay instead
                            let (row_count, col_count) = if self.players.len() > 2 {
                                (2, self.players.len().div_ceil(2))
                            } else {
                                (1, self.players.len())
                            };

                            let r = Rect::new(r.x, r.y, r.w / col_count as f32, r.h / row_count as f32);
                            let (w, h) = (r.w.min(r.h * ASPECT_MAX), r.h.min(r.w / ASPECT_MIN));
                            let ct = r.center();
                            let mut iter = self.players.iter_mut();
                            for i in 0..row_count {
                                for j in 0..iter.len().min(col_count) {
                                    let r = Rect::new(ct.x + j as f32 * r.w, ct.y + i as f32 * r.h, 0., 0.)
                                        .nonuniform_feather(w / 2., h / 2.)
                                        .feather(-0.01);
                                    let player = iter.next().unwrap();
                                    player.render(ui, r, self.game_scene.as_mut(), global_now, self.monitor_game_state)?;
                                }
                            }
                        } else {
                            // Room is not in Playing state, show waiting message
                            let ct = r.center();
                            ui.text("等待开始…")
                                .pos(ct.x, ct.y)
                                .anchor(0.5, 0.5)
                                .no_baseline()
                                .size(1.6)
                                .draw();
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
