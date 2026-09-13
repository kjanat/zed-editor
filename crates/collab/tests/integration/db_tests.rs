mod buffer_tests;
mod channel_tests;
mod db_tests;
mod extension_tests;

use std::sync::Arc;

pub use crate::test_db::TestDb;
use collab::db::*;
use collections::HashSet;

#[macro_export]
macro_rules! test_both_dbs {
    ($test_name:ident, $postgres_test_name:ident, $sqlite_test_name:ident) => {
        #[gpui::test]
        async fn $postgres_test_name(cx: &mut gpui::TestAppContext) {
            // In CI, only run postgres tests on Linux (where we have the postgres service).
            // Locally, always run them (assuming postgres is available).
            if std::env::var("CI").is_ok() && !cfg!(target_os = "linux") {
                return;
            }
            let test_db = $crate::db_tests::TestDb::postgres(cx.executor().clone());
            $test_name(test_db.db()).await;
        }

        #[gpui::test]
        async fn $sqlite_test_name(cx: &mut gpui::TestAppContext) {
            let test_db = $crate::db_tests::TestDb::sqlite(cx.executor().clone());
            $test_name(test_db.db()).await;
        }
    };
}

#[track_caller]
fn assert_channel_tree_matches(actual: Vec<Channel>, expected: Vec<Channel>) {
    let expected_channels = expected.into_iter().collect::<HashSet<_>>();
    let actual_channels = actual.into_iter().collect::<HashSet<_>>();
    pretty_assertions::assert_eq!(expected_channels, actual_channels);
}

fn channel_tree(channels: &[(ChannelId, &[ChannelId], &'static str)]) -> Vec<Channel> {
    use std::collections::HashMap;

    let mut result = Vec::new();
    let mut order_by_parent: HashMap<Vec<ChannelId>, i32> = HashMap::new();

    for (id, parent_path, name) in channels {
        let parent_key = parent_path.to_vec();
        let order = if parent_key.is_empty() {
            1
        } else {
            *order_by_parent
                .entry(parent_key.clone())
                .and_modify(|e| *e += 1)
                .or_insert(1)
        };

        result.push(Channel {
            id: *id,
            name: (*name).to_owned(),
            visibility: ChannelVisibility::Members,
            parent_path: parent_key,
            channel_order: order,
        });
    }

    result
}

async fn new_test_user(db: &Arc<Database>) -> UserId {
    db.create_user(false).await.unwrap().user_id
}
