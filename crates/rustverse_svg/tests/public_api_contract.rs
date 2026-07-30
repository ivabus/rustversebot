use nanoka::types::{BossSeasonDetail, SeasonDetail};
use rustverse::models::zzz::{ZZZDeadlyAssault, ZZZShiyuDefense};
use rustverse_svg::{TopDA, TopShiyu};

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
