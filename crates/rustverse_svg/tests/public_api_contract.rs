use nanoka::types::{BossSeasonDetail, SeasonDetail};
use rustverse::models::zzz::{ZZZDeadlyAssault, ZZZShiyuDefense};
use rustverse_svg::{
    TopDA, TopShiyu,
    gpu::{GpuRendererMetrics, GpuRendererService},
};

type Png = Vec<u8>;
type RenderResult = anyhow::Result<Png>;

// Keep the seven card entry points callable with their compatibility
// signatures while their implementation is moved between internal modules.
const _: fn(&TopDA) -> Png = rustverse_svg::top_da;
const _: fn(&TopShiyu) -> Png = rustverse_svg::top_shiyu;
const _: fn(&ZZZDeadlyAssault) -> Png = rustverse_svg::da;
const _: fn(&ZZZShiyuDefense) -> Png = rustverse_svg::shiyu;
const _: fn(&BossSeasonDetail) -> RenderResult = rustverse_svg::deadly_info;
const _: fn(&BossSeasonDetail, Option<&str>) -> RenderResult =
    rustverse_svg::deadly_info_with_begin_time;
const _: fn(&SeasonDetail) -> RenderResult = rustverse_svg::shiyu_info;

#[test]
fn all_card_entry_points_remain_public() {
    // The compile-time function-pointer assertions above are the test. This
    // named test keeps the API contract visible in integration-test output.
}

#[test]
fn gpu_metrics_snapshot_remains_public() {
    fn snapshot_is_callable(service: &GpuRendererService) {
        let _snapshot = service.metrics();
    }

    let _ = snapshot_is_callable as fn(&GpuRendererService);
    let metrics = GpuRendererMetrics::default();
    let _: u32 = metrics.pages;
    let _: u64 = metrics.allocated_bytes;
    let _: u64 = metrics.runtime_requests;
    let _: u64 = metrics.upload_count;
}
