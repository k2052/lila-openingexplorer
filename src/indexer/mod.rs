use std::{
    collections::{
        hash_map::{Entry, RandomState},
        HashMap,
    },
    hash::{BuildHasher, Hash, Hasher},
    sync::Arc,
    time::SystemTime,
};

use axum::http::StatusCode;
use clap::Clap;
use futures_util::StreamExt;
use rustc_hash::FxHashMap;
use shakmaty::{
    uci::Uci, variant::VariantPosition, zobrist::Zobrist, ByColor, CastlingMode, Outcome, Position,
};
use tokio::{
    sync::{
        mpsc::{self, error::TrySendError},
        watch, RwLock,
    },
    task::JoinHandle,
};

use crate::{
    db::Database,
    model::{
        GameInfo, GameInfoPlayer, Mode, Month, PersonalEntry, PersonalKeyBuilder, PersonalStatus,
        UserId,
    },
};

mod lila;

use lila::{Game, Lila};

#[derive(Clap, Clone)]
pub struct IndexerOpt {
    #[clap(long = "lila", default_value = "https://lichess.org")]
    lila: String,
    #[clap(long = "bearer")]
    bearer: Option<String>,
    #[clap(long = "indexers", default_value = "16")]
    indexers: usize,
}

#[derive(Clone)]
pub struct IndexerStub {
    db: Arc<Database>,
    indexing: Arc<RwLock<HashMap<UserId, watch::Sender<()>>>>,
    random_state: RandomState,
    txs: Vec<mpsc::Sender<IndexerMessage>>,
}

impl IndexerStub {
    pub fn spawn(db: Arc<Database>, opt: IndexerOpt) -> (IndexerStub, Vec<JoinHandle<()>>) {
        let random_state = RandomState::new();
        let indexing = Arc::new(RwLock::new(HashMap::new()));
        let mut txs = Vec::with_capacity(opt.indexers);
        let mut join_handles = Vec::with_capacity(opt.indexers);
        for idx in 0..opt.indexers {
            let (tx, rx) = mpsc::channel(500);
            txs.push(tx);
            join_handles.push(tokio::spawn(
                IndexerActor {
                    idx,
                    rx,
                    indexing: Arc::clone(&indexing),
                    db: Arc::clone(&db),
                    lila: Lila::new(opt.clone()),
                }
                .run(),
            ));
        }
        (
            IndexerStub {
                db,
                random_state,
                indexing,
                txs,
            },
            join_handles,
        )
    }

    pub async fn index_player(&self, player: &UserId) -> Option<watch::Receiver<()>> {
        // Optimization: First try subscribing to an existing indexing run,
        // without acquiring a write lock.
        {
            let guard = self.indexing.read().await;
            if let Some(sender) = guard.get(player) {
                return Some(sender.subscribe());
            }
        }

        // Check player indexing status.
        let mut status = self
            .db
            .queryable()
            .get_player_status(player)
            .expect("get player status")
            .unwrap_or_default();

        let since_created_at = match status
            .maybe_revisit_ongoing()
            .or_else(|| status.maybe_index())
        {
            Some(since) => since,
            None => return None, // Do not reindex so soon!
        };

        // Queue indexing request.
        let responsible_indexer = {
            let mut hasher = self.random_state.build_hasher();
            player.hash(&mut hasher);
            hasher.finish() as usize % self.txs.len()
        };

        let mut guard = self.indexing.write().await;
        let entry = match guard.entry(player.to_owned()) {
            Entry::Occupied(entry) => return Some(entry.get().subscribe()),
            Entry::Vacant(entry) => entry,
        };

        match self.txs[responsible_indexer].try_send(IndexerMessage::IndexPlayer {
            player: player.to_owned(),
            status,
            since_created_at,
        }) {
            Ok(_) => {
                let (sender, receiver) = watch::channel(());
                entry.insert(sender);
                Some(receiver)
            }
            Err(TrySendError::Full(_)) => {
                log::error!(
                    "indexer {}: not queuing {} because indexer queue is full",
                    responsible_indexer,
                    player.as_str()
                );
                None
            }
            Err(TrySendError::Closed(_)) => panic!("indexer {} died", responsible_indexer),
        }
    }
}

struct IndexerActor {
    idx: usize,
    indexing: Arc<RwLock<HashMap<UserId, watch::Sender<()>>>>,
    rx: mpsc::Receiver<IndexerMessage>,
    db: Arc<Database>,
    lila: Lila,
}

impl IndexerActor {
    async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                IndexerMessage::IndexPlayer { player, status, since_created_at } => {
                    self.index_player(&player, status, since_created_at).await;

                    let mut guard = self.indexing.write().await;
                    guard.remove(&player);
                }
            }
        }
    }

    async fn index_player(&self, player: &UserId, mut status: PersonalStatus, since_created_at: u64) {
        log::info!(
            "indexer {} starting {} (created_at >= {})",
            self.idx,
            player.as_str(),
            since_created_at
        );
        let mut games = match self.lila.user_games(player, since_created_at).await {
            Ok(games) => games,
            Err(err) if err.status() == Some(StatusCode::NOT_FOUND) => {
                log::warn!("indexer did not find player {}", player.as_str());
                return;
            }
            Err(err) => {
                log::error!("indexer {}: request failed: {}", self.idx, err);
                return;
            }
        };

        let hash = ByColor::new_with(|color| PersonalKeyBuilder::with_user_pov(&player, color));
        let mut num_games = 0;
        while let Some(game) = games.next().await {
            let game = match game {
                Ok(game) => game,
                Err(err) => {
                    log::error!("{}", err);
                    continue;
                }
            };

            self.index_game(player, &hash, game, &mut status);

            num_games += 1;
            if num_games % 1024 == 0 {
                log::info!(
                    "indexer {}: indexed {} games for {}",
                    self.idx,
                    num_games,
                    player.as_str()
                );
            }
        }

        status.indexed_at = SystemTime::now();
        self.db
            .queryable()
            .put_player_status(player, status)
            .expect("put player status");
        log::info!(
            "indexer {}: finished indexing {} games for {}",
            self.idx,
            num_games,
            player.as_str()
        );
    }

    fn index_game(
        &self,
        player: &UserId,
        hash: &ByColor<PersonalKeyBuilder>,
        game: Game,
        status: &mut PersonalStatus,
    ) {
        status.latest_created_at = game.created_at;

        if game.status.is_ongoing() {
            if status.revisit_ongoing_created_at.is_none() {
                log::debug!("will revisit ongoing game {} eventually", game.id);
                status.revisit_ongoing_created_at = Some(game.created_at);
            }
            return;
        }

        if game.status.is_unindexable() {
            log::debug!("not indexing {} with status {:?}", game.id, game.status);
            return;
        }

        if game.players.any(|p| p.user.is_none()) {
            return;
        }

        let color = match game
            .players
            .find(|p| p.user.as_ref().map_or(false, |user| user.name == *player))
        {
            Some(color) => color,
            None => {
                log::error!("{} did not play in {}", player.as_str(), game.id);
                return;
            }
        };

        let month = Month::from_time_saturating(game.last_move_at);
        let outcome = Outcome::from_winner(game.winner);

        let queryable = self.db.queryable();
        if queryable
            .get_game_info(game.id)
            .expect("get game info")
            .map_or(false, |info| info.indexed.into_color(color))
        {
            log::debug!(
                "{}/{} already indexed",
                game.id,
                color.fold("white", "black")
            );
            return;
        }
        queryable
            .merge_game_info(
                game.id,
                GameInfo {
                    winner: outcome.winner(),
                    speed: game.speed,
                    rated: game.rated,
                    month,
                    players: game.players.map(|p| GameInfoPlayer {
                        name: p.user.map(|p| p.name.to_string()),
                        rating: p.rating,
                    }),
                    indexed: ByColor::new_with(|c| color == c),
                },
            )
            .expect("put game info");

        let variant = game.variant.into();
        let pos = match game.initial_fen {
            Some(fen) => VariantPosition::from_setup(variant, &fen, CastlingMode::Chess960),
            None => Ok(VariantPosition::new(variant)),
        };

        let mut pos: Zobrist<_, u128> = match pos {
            Ok(pos) => Zobrist::new(pos),
            Err(err) => {
                log::warn!("not indexing {}: {}", game.id, err);
                return;
            }
        };

        // Build an intermediate table to remove loops (due to repetitions).
        let mut table: FxHashMap<u128, Uci> =
            FxHashMap::with_capacity_and_hasher(game.moves.len(), Default::default());

        for (ply, san) in game.moves.into_iter().enumerate() {
            let m = match san.to_move(&pos) {
                Ok(m) => m,
                Err(err) => {
                    log::warn!("cutting off {} at ply {}: {}: {}", game.id, ply, err, san);
                    break;
                }
            };

            let uci = m.to_uci(CastlingMode::Chess960);
            table.insert(pos.zobrist_hash(), uci);

            pos.play_unchecked(&m);
        }

        for (zobrist, uci) in table {
            queryable
                .merge_personal(
                    hash.by_color(color)
                        .with_zobrist(variant, zobrist)
                        .with_month(month),
                    PersonalEntry::new_single(
                        uci.clone(),
                        game.speed,
                        Mode::from_rated(game.rated),
                        game.id,
                        outcome,
                    ),
                )
                .expect("merge personal");
        }
    }
}

enum IndexerMessage {
    IndexPlayer { player: UserId, status: PersonalStatus, since_created_at: u64 },
}
