use benchmarks::bench_utils::random_rust_file;
use buffer_diff::BufferDiff;
use gpui::{App, AppContext as _, BenchAppContext, Entity};
use language::{Anchor, Buffer, BufferSnapshot, Capability, OffsetRangeExt as _, Point};
use multi_buffer::{ExcerptRange, MultiBuffer, PathKey, build_excerpt_ranges};
use rand::{SeedableRng as _, rngs::StdRng};
use settings::SettingsStore;
use util::rel_path::rel_path;

const CONTEXT_LINE_COUNT: u32 = 3;
const LINES_PER_HUNK: usize = 10;

struct DiffFixture {
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
}

fn diff_fixture(hunk_count: usize, cx: &mut BenchAppContext) -> DiffFixture {
    let mut rng = StdRng::seed_from_u64(155);
    let base_lines = random_rust_file(&mut rng, hunk_count * LINES_PER_HUNK + 15);
    let current_text = base_lines
        .iter()
        .enumerate()
        .map(|(row, line)| {
            if row % LINES_PER_HUNK == LINES_PER_HUNK / 2 {
                format!("{line} // changed")
            } else {
                line.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let base_text = base_lines.join("\n");
    cx.update(|cx| {
        let buffer = cx.new(|cx| Buffer::local(current_text, cx));
        let diff = cx.new(|cx| {
            BufferDiff::new_with_base_text(&base_text, &buffer.read(cx).text_snapshot(), cx)
        });
        DiffFixture { buffer, diff }
    })
}

fn prepare_excerpt_ranges(
    fixture: &DiffFixture,
    cx: &App,
) -> (BufferSnapshot, Vec<ExcerptRange<Point>>) {
    let snapshot = fixture.buffer.read(cx).snapshot();
    let diff_snapshot = fixture.diff.read(cx).snapshot(cx);
    let hunk_ranges = diff_snapshot
        .hunks_intersecting_range(
            Anchor::min_max_range_for_buffer(snapshot.remote_id()),
            &snapshot,
        )
        .map(|hunk| hunk.buffer_range.to_point(&snapshot))
        .collect::<Vec<_>>();
    let excerpt_ranges = build_excerpt_ranges(hunk_ranges, CONTEXT_LINE_COUNT, &snapshot);
    (snapshot, excerpt_ranges)
}

#[gpui::bench(
    inputs = hunk_counts(),
    group = "Diff excerpt preparation",
    input_name = "hunks",
    sample_size = 10
)]
fn diff_excerpt_preparation(hunk_count: &usize, cx: &mut BenchAppContext) {
    init_context(cx);
    let fixture = diff_fixture(*hunk_count, cx);

    cx.bench_iter(|cx| {
        cx.update(|cx| {
            prepare_excerpt_ranges(&fixture, cx);
        });
    });
}

#[gpui::bench(
    inputs = hunk_counts(),
    group = "Diff excerpt install",
    input_name = "hunks",
    sample_size = 10
)]
fn diff_excerpt_install(hunk_count: &usize, cx: &mut BenchAppContext) {
    init_context(cx);
    let fixture = diff_fixture(*hunk_count, cx);
    let (snapshot, excerpt_ranges) = cx.update(|cx| prepare_excerpt_ranges(&fixture, cx));

    cx.bench_iter(|cx| {
        cx.update(|cx| {
            let multibuffer = cx.new(|_| MultiBuffer::new(Capability::ReadWrite));
            multibuffer.update(cx, |multibuffer, cx| {
                multibuffer.set_all_diff_hunks_expanded(cx);
                multibuffer.update_excerpt_ranges_for_path(
                    PathKey::with_sort_prefix(0, rel_path("file.rs").into_arc()),
                    fixture.buffer.clone(),
                    &snapshot,
                    excerpt_ranges.clone(),
                    cx,
                );
                multibuffer.add_diff(fixture.diff.clone(), cx);
            });
        });
    });
}

fn init_context(cx: &mut BenchAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
    });
}

fn hunk_counts() -> Vec<usize> {
    vec![100, 1_000, 10_000]
}

gpui::bench_group!(benches, diff_excerpt_preparation, diff_excerpt_install);
gpui::bench_main!(benches);
