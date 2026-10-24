"use strict";

// Backtest (#strategy/<kind>): the portfolio's price strategies on history with the
// same lot planner, trade band and risk stops as in trading. The supervisor runs
// `tinkoff-bot backtest` in the background; the report comes with the command.
$("strategy-log").insertAdjacentHTML("beforebegin", `<div id="backtest-panel" class="backtest-panel"><div class="log-heading"><div><h3>Проверка на истории</h3><p class="detail-note">Те же стратегии, расчёт лотов, порог сделки и стопы, что в торговле. Исполнение — по цене открытия с проскальзыванием, комиссия — по ставке контура. Стратегии на отчётности, дивидендах и решениях ИИ на истории не проверяются.</p></div></div><form id="backtest-form" class="inline-form backtest-form"><label for="backtest-from">С</label><input id="backtest-from" type="date" required><label for="backtest-to">по</label><input id="backtest-to" type="date" required><label for="backtest-slippage">Проскальзывание, б.п.</label><input id="backtest-slippage" type="number" min="0" max="100" step="1" value="5" required><button class="button" type="submit">Проверить</button></form><div id="backtest-result" role="status"></div></div>`);

(()=>{
  const day=offset=>{const d=new Date(Date.now()-offset*86400000);return d.toISOString().slice(0,10);};
  $("backtest-from").value=day(366);$("backtest-to").value=day(1);
})();

function backtestRun(c) {
  return (controlsData?.commands || []).find(x=>x.kind==="backtest" && x.payload?.strategy_id===c.strategy_id);
}

function backtestReport(r) {
  const pct=(v,d=2)=>v==null ? "—" : `${v>0 ? "+" : ""}${fmt(v,d)}%`, ratio=v=>v==null ? "—" : fmt(v,2);
  const curve=(r.curve_sample || []).map(([date,equity])=>[date,(equity/Number(r.capital)-1)*100]);
  const reasons=Object.entries(r.skip_reasons || {});
  return `<dl class="portfolio-facts backtest-facts"><div><dt>Доходность</dt><dd>${pct(r.return_percent)}${r.annual_return_percent!=null ? `<span class="subtext">годовых ${pct(r.annual_return_percent)}</span>` : ""}</dd></div><div><dt>${r.benchmark ? "К ориентиру" : "Ориентир"}</dt><dd>${r.benchmark ? `${pct(r.benchmark.excess_percent)}<span class="subtext">ориентир ${pct(r.benchmark.return_percent)}</span>` : "—"}</dd></div><div><dt>Макс. просадка</dt><dd>${fmt(r.max_drawdown_percent,2)}%${r.worst_day_percent!=null ? `<span class="subtext">худший день ${pct(r.worst_day_percent)}</span>` : ""}</dd></div><div><dt>Шарп / Сортино</dt><dd>${ratio(r.sharpe)} / ${ratio(r.sortino)}${r.volatility_percent!=null ? `<span class="subtext">волатильность ${fmt(r.volatility_percent,1)}%</span>` : ""}</dd></div><div><dt>Сделки</dt><dd>${fmt(r.trades)}<span class="subtext">оборот ${fmt(Number(r.turnover_rub),0)} ₽</span></dd></div><div><dt>Издержки</dt><dd>${fmt(Number(r.commission_rub)+Number(r.slippage_rub),0)} ₽<span class="subtext">комиссия ${fmt(Number(r.commission_rub),0)} · проскальзывание ${fmt(Number(r.slippage_rub),0)}</span></dd></div><div><dt>Сессии</dt><dd>${fmt(r.sessions)}${r.risk_blocked_sessions ? `<span class="subtext warning">стоп сработал: ${fmt(r.risk_blocked_sessions)}</span>` : ""}</dd></div><div><dt>Решения</dt><dd>${fmt(r.decisions)}${r.skipped_decisions ? `<span class="subtext warning">не исполнено ${fmt(r.skipped_decisions)}</span>` : ""}</dd></div></dl>${typeof sparkline==="function" ? sparkline(curve) : ""}${reasons.length ? `<p class="detail-note">Не исполнено: ${reasons.map(([reason,count])=>`${esc(reason)} — ${fmt(count)}`).join("; ")}</p>` : ""}<p class="detail-note">${esc(r.from)} — ${esc(r.to)} · капитал ${fmt(Number(r.capital),0)} ₽ → ${fmt(Number(r.final_equity),0)} ₽ · Шарп и Сортино годовые по дневным изменениям, без безрисковой ставки.</p>`;
}

function renderBacktest(c) {
  if(!c){$("backtest-result").innerHTML="";return;}
  const run=backtestRun(c);
  let html="";
  if(run && ["queued","running"].includes(run.status))html=`<p class="detail-note">${esc(run.result?.message || "Проверка поставлена в очередь…")} ${esc(run.payload.from)} — ${esc(run.payload.to)}</p>`;
  else if(run?.status==="failed")html=`<p class="detail-note warning-text">${esc(run.result?.message || "Проверка не удалась")}</p>`;
  else if(run?.result?.report)html=backtestReport(run.result.report);
  if($("backtest-result").dataset.html!==html){$("backtest-result").innerHTML=html;$("backtest-result").dataset.html=html;}
  $("backtest-form").querySelector("button").disabled=Boolean(run && ["queued","running"].includes(run.status));
}

$("backtest-form").addEventListener("submit",e=>{
  e.preventDefault();
  const c=data && selectedCase();
  if(c)issue("backtest",{strategy_id:c.strategy_id,from:$("backtest-from").value,to:$("backtest-to").value,slippage_bps:Number($("backtest-slippage").value)},true);
});
