"use strict";

let detailKind = null, marketSeries = null, marketKey = "", marketRequest = 0, marketFetched = 0;
let chartCandles = [], visibleCandles = [], assetMarkers = new Map(), highlightedTrade = null, tradeFocus = null;
// Visible window of the loaded series: `count` candles ending before `end`. While `follow`
// is set the newest candle stays in view; after a pan the `anchor` candle keeps its place.
const chartView = {key:"", count:0, end:0, follow:true, anchor:null};
let chartGeometry = null, chartPointer = null, chartDrag = null;
const intervalMilliseconds = {"5s":5000,"10s":10000,"30s":30000,"5m":300000,"30m":1800000,day:86400000};
const MIN_VISIBLE = 15, MAX_VISIBLE = 600;
const pendingStates = new Set(["pending", "submitting", "accepted", "uncertain"]);
const detailStatuses = {pending:"В плане",submitting:"Отправляется",accepted:"У брокера",uncertain:"Проверка исполнения",filled:"Исполнена",cancelled:"Отменена",rejected:"Отклонена"};
const clamp = (value, low, high) => Math.min(high, Math.max(low, value));
const clockSeconds = value => new Intl.DateTimeFormat("ru-RU", {timeZone:"Europe/Moscow", hour:"2-digit", minute:"2-digit", second:"2-digit"}).format(new Date(value));
const priceText = value => fmt(value, Math.abs(value) < 1 ? 4 : Math.abs(value) < 10 ? 3 : 2);

function chartInterval() { return $("asset-interval").dataset.value; }
function setChartInterval(value) {
  $("asset-interval").dataset.value = value;
  for (const button of $("asset-interval").querySelectorAll("[data-interval]")) button.setAttribute("aria-checked", String(button.dataset.interval === value));
}

// A strategy has its own page in the «Стратегии» section (#strategy/<name>).
function chooseStrategy(kind) {
  $("strategy-filter").value = kind;
  renderStrategies();renderActivity();renderAccounts();renderDetail();
  const hash = kind ? "#strategy/" + encodeURIComponent(kind) : "#strategies";
  if(location.hash !== hash)history.pushState(null, "", hash);
  if(typeof applyView === "function"){applyView();scrollTo(0, 0);}
}

function detailOrders() {
  const c = selectedCase(), uid = $("asset-select").value;
  return c ? data.orders.filter(o => o.strategy === c.kind && o.instrument_id === uid) : [];
}

function plannedOrders() {
  const targets = new Map((data.targets || []).map(t => [t.batch_id,t]));
  return detailOrders().filter(o => {
    if(!pendingStates.has(o.status))return false;
    const target = targets.get(o.batch_id);
    // Submitting/accepted/uncertain still need reconciliation after expiry.
    if(o.status !== "pending")return true;
    return target && !["failed","completed"].includes(target.status) && Date.parse(target.valid_until) > Date.now();
  });
}

function executedFills() {
  const c=selectedCase(),uid=$("asset-select").value;
  return c ? data.fills.filter(f => f.strategy === c.kind && f.instrument_id === uid) : [];
}

const tradeKey = f => f.trade_id ? "trade:"+f.trade_id : "fill:"+f.request_id+":"+f.executed_at;

// Fills with a confirmed price, plus executed orders whose fill price has not arrived yet.
function tradeRows() {
  const fills=executedFills(), filledRequests=new Set(fills.map(f=>f.request_id));
  const executed=detailOrders().filter(o => !filledRequests.has(o.request_id) && (o.status === "filled" || Number(o.lots_executed)>0));
  return fills.map(f=>({key:tradeKey(f),time:f.executed_at,side:f.side,lots:f.lots,price:Number(f.price)>0 && Number.isFinite(Number(f.price)) ? Number(f.price) : null,maintenance:f.maintenance,manual:f.manual,confirmed:true}))
    .concat(executed.map(o=>({key:"order:"+o.request_id,time:o.updated_at,side:o.side,lots:o.lots_executed,price:null,maintenance:o.maintenance,manual:o.manual,confirmed:false})))
    .filter(r=>Number.isFinite(Date.parse(r.time)))
    .sort((a,b)=>String(b.time).localeCompare(a.time));
}

// Time span of the whole loaded series, not only of the visible window.
function chartRange() {
  if(!chartCandles.length || !marketSeries)return null;
  return [Date.parse(chartCandles[0].time),Date.parse(chartCandles.at(-1).time)+intervalMilliseconds[marketSeries.interval]];
}

// Index of the candle containing `time` (the last one that starts at or before it).
function candleIndexAt(time) {
  const stamp=Date.parse(time);let low=0,high=chartCandles.length-1,found=0;
  while(low<=high){const middle=(low+high)>>1;if(Date.parse(chartCandles[middle].time)<=stamp){found=middle;low=middle+1;}else high=middle-1;}
  return found;
}

// Highlight one trade; switch to a longer candle interval when it is outside the loaded one.
function showTrade(key) {
  const row=tradeRows().find(r=>r.key===key);if(!row)return;
  highlightedTrade=key;tradeFocus=key;
  const time=Date.parse(row.time), range=chartRange(), sameAsset=marketSeries?.uid===$("asset-select").value;
  if(sameAsset && range && time>=range[0] && time<=range[1])renderAssetChart();
  else{setChartInterval(Date.now()-time<13*86400000 ? "30m" : "day");loadMarket(true);}
  $("asset-chart").scrollIntoView({behavior:"smooth",block:"center"});
}

function showTradeOnChart(kind,uid,key) {
  if($("strategy-filter").value!==kind)chooseStrategy(kind);
  if([...$("asset-select").options].some(o=>o.value===uid)){$("asset-select").value=uid;renderDetailTrades();}
  marketSeries=null;showTrade(key);
}

function renderDetail() {
  const c=selectedCase();
  $("strategy-detail").hidden=!c;
  if(!c){detailKind=null;marketRequest++;return;}
  const changed=detailKind!==c.kind;
  detailKind=c.kind;
  // What the process is doing right now is shown in #strategy-now; this line is the schedule only.
  $("detail-name").textContent=displayName(c);
  $("detail-schedule").textContent=`${c.title && c.title!==c.kind ? c.kind+" · " : ""}Расписание: ${scheduleSummary(c)}${c.launch_at ? ` · ближайшее окно: прогрев ${datetime(c.launch_at)}, торговля ${datetime(c.opening_at)} – ${datetime(c.window_end_at)} МСК` : ""}${c.predecessor && !c.owns_account ? " · перед запуском ожидается освобождение счёта" : ""}`;
  const old=$("asset-select").value;
  const chartAssets=typeof allAssets === "function" ? allAssets(c) : c.assets || [];
  $("asset-select").innerHTML=chartAssets.map(a=>`<option value="${esc(a.uid)}">${esc(a.ticker)}${a.benchmark ? " · ориентир рынка" : ""}</option>`).join("");
  if(!changed && chartAssets.some(a=>a.uid===old))$("asset-select").value=old;
  if(changed)setChartInterval(c.kind.includes("intraday") ? "30m" : "day");
  if(changed)highlightedTrade=null;
  renderDetailTrades();loadMarket();
  if(typeof renderControls === "function")renderControls();
  if(typeof loadLogs === "function")loadLogs(changed);
}

function renderDetailTrades() {
  const c=selectedCase();if(!c)return;
  const rows=tradeRows();
  $("executed-count").textContent=rows.length;
  $("executed-trades").innerHTML=rows.length ? table(["Время, МСК","Сделка","Цена, ₽"],rows.map(r=>`<tr class="${r.key===highlightedTrade ? "highlighted" : ""}"><td>${esc(datetime(r.time))}${!r.confirmed ? '<span class="subtext">Время сверки заявки</span>' : ""}</td><td>${side(r.side)} · ${fmt(r.lots)} лот.${r.maintenance ? '<span class="subtext warning">Служебное закрытие</span>' : r.manual ? '<span class="subtext">Ручная сделка</span>' : ""}<button type="button" class="link-button" data-show-trade="${esc(r.key)}">Показать на графике</button></td><td>${r.price==null ? '<span class="muted-text">Не получена</span>' : fmt(r.price,2)}</td></tr>`)) : empty("Сделок по этому активу ещё нет","После исполнения здесь появятся лоты и подтверждённая цена, а на графике — отметки ▲ покупки и ▼ продажи.");
  const plans=plannedOrders();
  $("planned-count").textContent=plans.length;
  $("planned-trades").innerHTML=plans.length ? table(["Направление","Лоты","Цена","Состояние"],plans.map(o=>`<tr><td>${side(o.side)}${o.maintenance ? '<span class="subtext warning">Служебное закрытие</span>' : ""}</td><td>${fmt(o.lots)}<span class="subtext">Цель: ${fmt(o.target_lots)} лот.</span></td><td>${o.limit_price==null ? "По рынку" : fmt(o.limit_price,2)}</td><td>${badge(detailStatuses[o.status] || o.status,o.status === "uncertain" ? "warn" : "waiting")}</td></tr>`)) : empty("Запланированных заявок нет","Стратегия ещё не сохранила актуальную заявку по этому активу.");
  const latest=(data.targets || []).filter(t=>t.strategy===c.kind && !t.maintenance).sort((a,b)=>String(b.generated_at).localeCompare(a.generated_at))[0];
  const weight=latest?.weights.find(w=>w.instrument_id===$("asset-select").value);
  $("target-summary").textContent=weight ? `Последняя цель: ${fmt(weight.weight*100,2)}% портфеля · ${datetime(latest.generated_at)} МСК${Date.parse(latest.valid_until) <= Date.now() ? " · срок исполнения истёк" : ""}. Целевая доля не является обещанием сделки.` : "Будущий сигнал неизвестен. Здесь отображаются только сохранённые планы, без прогнозирования новых сделок.";
}

async function loadMarket(force=false) {
  const uid=$("asset-select").value,interval=chartInterval(),key=`${uid}:${interval}`;
  if(!uid){$("asset-empty").hidden=false;$("asset-empty").textContent="Для стратегии пока не определены инструменты.";return;}
  if(!force && key===marketKey && Date.now()-marketFetched<5000){renderAssetChart();return;}
  const changed=key!==marketKey;
  marketKey=key;marketFetched=Date.now();
  if(changed){marketSeries=null;chartCandles=[];chartGeometry=null;$("asset-chart").replaceChildren();$("asset-empty").hidden=false;$("asset-empty").textContent="Загружаем свечи инструмента…";$("asset-price").textContent="—";$("asset-time").textContent="";$("asset-ohlc").textContent="";$("chart-range").textContent="";}
  const request=++marketRequest;
  try{
    const response=await fetch(`/api/market?instrument=${encodeURIComponent(uid)}&interval=${interval}`,{cache:"no-store",signal:AbortSignal.timeout(8000)});
    if(!response.ok)throw Error("no chart");
    const result=await response.json();if(request!==marketRequest)return;
    marketSeries=result;renderAssetChart();
  }catch(_){
    if(request!==marketRequest)return;
    if(!marketSeries){$("asset-empty").hidden=false;$("asset-empty").textContent="История ещё загружается или источник недоступен. Повторим автоматически.";}
    $("asset-freshness").textContent="Нет свежего ответа по котировкам";
  }
}

// Visible window for the current series; a new instrument or interval starts at the newest candle.
function updateView(plotWidth) {
  const len=chartCandles.length, preferred=Math.round(clamp(plotWidth/9,20,160));
  if(chartView.key!==marketKey)Object.assign(chartView,{key:marketKey,count:Math.min(len,preferred),end:len,follow:true,anchor:null});
  else if(chartView.follow)chartView.end=len;
  else if(chartView.anchor)chartView.end=candleIndexAt(chartView.anchor)+1;
  chartView.count=clamp(chartView.count,Math.min(MIN_VISIBLE,len),Math.min(MAX_VISIBLE,len));
  chartView.end=clamp(chartView.end,chartView.count,len);
}

function moveView(end,count=chartView.count) {
  const len=chartCandles.length;if(!len)return;
  count=clamp(Math.round(count),Math.min(MIN_VISIBLE,len),Math.min(MAX_VISIBLE,len));
  end=clamp(Math.round(end),count,len);
  Object.assign(chartView,{count,end,follow:end===len,anchor:chartCandles[end-1].time});
  renderAssetChart();
}
const panBy = delta => moveView(chartView.end+delta);
// Zoom keeps the candle under the cursor (or the right edge) at the same place on screen.
function zoomBy(factor,anchor=chartView.end-1) {
  const count=chartView.count, start=chartView.end-count, relative=(anchor-start+.5)/count;
  const next=clamp(Math.round(count*factor),Math.min(MIN_VISIBLE,chartCandles.length),Math.min(MAX_VISIBLE,chartCandles.length));
  if(next===count)return;
  moveView(chartView.follow && anchor>=chartView.end-1 ? chartCandles.length : anchor+.5+(1-relative)*next,next);
}

function niceStep(raw) {
  const power=10**Math.floor(Math.log10(raw)), unit=raw/power;
  return (unit<=1 ? 1 : unit<=2 ? 2 : unit<=2.5 ? 2.5 : unit<=5 ? 5 : 10)*power;
}
function tickLabel(time,previous) {
  const interval=marketSeries.interval;
  if(interval==="day")return previous && new Date(previous).getUTCFullYear()===new Date(time).getUTCFullYear() ? date(time) : date(time,{year:"numeric"});
  const clock=interval.endsWith("s") ? clockSeconds(time) : date(time,{day:undefined,month:undefined,hour:"2-digit",minute:"2-digit"});
  return previous && date(previous)===date(time) ? clock : `${date(time)} ${clock}`;
}
const pointerTime = time => marketSeries.interval==="day" ? date(time,{year:"numeric"}) : `${date(time)} ${marketSeries.interval.endsWith("s") ? clockSeconds(time) : date(time,{day:undefined,month:undefined,hour:"2-digit",minute:"2-digit"})}`;

function renderAssetChart() {
  if(!marketSeries || !selectedCase())return;
  chartCandles=marketSeries.candles.filter(c=>[c.open,c.high,c.low,c.close].every(Number.isFinite) && Number.isFinite(Date.parse(c.time)));
  const svg=$("asset-chart"), plans=plannedOrders(), trades=tradeRows();
  $("asset-empty").hidden=chartCandles.length>0;
  if(!chartCandles.length){svg.replaceChildren();chartGeometry=null;visibleCandles=[];$("asset-ohlc").textContent="";$("chart-range").textContent="";renderTradeLegend(trades,0,0,0,plans);$("asset-empty").textContent=marketSeries.loading ? "История загружается…" : marketSeries.retry_at ? "Источник ограничил запросы; повторим загрузку автоматически." : "Свечей в выбранном периоде нет.";return;}
  const last=chartCandles.at(-1),asset=(typeof allAssets === "function" ? allAssets() : selectedCase().assets).find(a=>a.uid===$("asset-select").value);
  $("asset-price").textContent=`${priceText(last.close)} ₽`;
  $("asset-time").textContent=`${asset?.ticker || ""} · ${pointerTime(last.time)} МСК${last.complete ? " · закрытие свечи" : " · свеча формируется"}`;
  $("asset-freshness").textContent=`Котировки: ${ageText(marketSeries.observed_at)}${marketSeries.error || age(marketSeries.observed_at)>180 ? " · обновление задерживается" : ""}`;
  // Draw in real pixels of the chart box: candles and labels stay crisp at any width.
  const box=svg.getBoundingClientRect(), W=Math.max(320,Math.round(box.width) || 900), H=Math.max(220,Math.round(box.height) || 360);
  svg.setAttribute("viewBox",`0 0 ${W} ${H}`);
  const left=8, right=W-74, top=12, bottom=H-64, axis=bottom+36;
  updateView(right-left);
  const [seriesStart,seriesEnd]=chartRange(), inSeries=trades.filter(r=>{const t=Date.parse(r.time);return t>=seriesStart && t<=seriesEnd;});
  // A trade chosen from a table is centred once; outside shorter series fall back to day candles.
  if(tradeFocus && marketSeries.uid===$("asset-select").value && !marketSeries.loading){
    const focused=inSeries.find(r=>r.key===tradeFocus);
    if(focused){const index=candleIndexAt(focused.time);chartView.end=clamp(index+Math.ceil(chartView.count/2),chartView.count,chartCandles.length);chartView.follow=chartView.end===chartCandles.length;chartView.anchor=chartCandles[chartView.end-1].time;tradeFocus=null;}
    else if(marketSeries.interval!=="day"){setChartInterval("day");setTimeout(()=>loadMarket(true));}
    else tradeFocus=null;
  }
  const start=chartView.end-chartView.count, end=chartView.end, count=chartView.count, step=(right-left)/count;
  visibleCandles=chartCandles.slice(start,end);
  const inView=inSeries.map(r=>({...r,index:candleIndexAt(r.time)})).filter(r=>r.index>=start && r.index<end);
  const priced=inView.filter(r=>r.price!=null), unpriced=inView.filter(r=>r.price==null);
  const levels=plans.map(o=>Number(o.limit_price)).filter(p=>p>0);
  const lows=visibleCandles.map(c=>c.low).concat(priced.map(r=>r.price),levels),highs=visibleCandles.map(c=>c.high).concat(priced.map(r=>r.price),levels);
  const low=Math.min(...lows),high=Math.max(...highs),padding=Math.max((high-low)*.06,high*.0005,.0001),min=low-padding,max=high+padding;
  const y=price=>bottom-(price-min)/(max-min)*(bottom-top), x=index=>left+(index-start+.5)*step;
  let content=`<rect x="${left}" y="${top}" width="${right-left}" height="${bottom-top}" class="plot-area"/>`;
  // Price axis on the right with round levels.
  const priceStep=niceStep((max-min)/6);
  for(let level=Math.ceil(min/priceStep)*priceStep;level<=max;level+=priceStep){const yy=y(level);content+=`<line x1="${left}" x2="${right}" y1="${yy}" y2="${yy}" class="chart-grid"/><text x="${right+8}" y="${yy+3.5}" class="chart-label">${esc(priceText(level))}</text>`;}
  // Time axis: stable ticks every k candles, day separators for intraday candles.
  const every=Math.max(1,Math.ceil(96/step));let previousTick=null;
  content+=`<line x1="${left}" x2="${right}" y1="${axis}" y2="${axis}" class="chart-axis"/>`;
  for(let index=start;index<end;index++){
    const c=chartCandles[index], xx=x(index);
    if(index>start && marketSeries.interval!=="day" && date(chartCandles[index-1].time)!==date(c.time))content+=`<line x1="${xx-step/2}" x2="${xx-step/2}" y1="${top}" y2="${bottom}" class="day-separator"/>`;
    if(index%every===0 && xx>left+30 && xx<right-30){content+=`<line x1="${xx}" x2="${xx}" y1="${top}" y2="${bottom}" class="chart-grid vertical"/><line x1="${xx}" x2="${xx}" y1="${axis}" y2="${axis+4}" class="chart-axis"/><text x="${xx}" y="${axis+16}" text-anchor="middle" class="chart-label">${esc(tickLabel(c.time,previousTick))}</text>`;previousTick=c.time;}
  }
  const body=clamp(step*.68,1,16);
  content+=visibleCandles.map((c,offset)=>{const xx=x(start+offset),cls=`${c.close>=c.open ? "candle-up" : "candle-down"}${c.complete ? "" : " forming"}`;const bodyTop=Math.min(y(c.open),y(c.close));return `<line x1="${xx}" x2="${xx}" y1="${y(c.high)}" y2="${y(c.low)}" class="wick ${cls}"/><rect x="${xx-body/2}" y="${bodyTop}" width="${body}" height="${Math.max(1,Math.abs(y(c.open)-y(c.close)))}" class="${cls}"/>`;}).join("");
  // Last price: dashed level across the plot and a box on the price axis.
  if(last.close>=min && last.close<=max){const yy=y(last.close),cls=last.close>=last.open ? "up" : "down";content+=`<line x1="${left}" x2="${right}" y1="${yy}" y2="${yy}" class="last-price-line ${cls}"/><rect x="${right+2}" y="${yy-9}" width="70" height="18" rx="3" class="last-price-box ${cls}"/><text x="${right+8}" y="${yy+4}" class="last-price-text">${esc(priceText(last.close))}</text>`;}
  assetMarkers=new Map();
  const addMarker=(index,text)=>assetMarkers.set(index,[...(assetMarkers.get(index) || []),text]);
  const labels=priced.length<=30;
  for(const r of priced){
    const xx=x(r.index),yy=y(r.price),buy=/buy/i.test(r.side),delta=buy ? 1 : -1,direction=buy ? "buy" : "sell";
    const tip=`${datetime(r.time)} · ${buy ? "Покупка" : "Продажа"}: ${fmt(r.lots)} лот. × ${fmt(r.price,2)} ₽${r.maintenance ? " · служебное закрытие" : r.manual ? " · ручная сделка" : ""}`;
    content+=`<g class="trade-mark" data-trade="${esc(r.key)}"><line x1="${xx}" x2="${xx}" y1="${yy}" y2="${bottom}" class="trade-guide ${direction}"/>${r.key===highlightedTrade ? `<circle cx="${xx}" cy="${yy}" r="15" class="trade-highlight"/>` : ""}<path d="M ${xx} ${yy-10*delta} L ${xx-8} ${yy+7*delta} L ${xx+8} ${yy+7*delta} Z" class="trade-${direction}${r.maintenance ? " trade-maintenance" : r.manual ? " trade-manual" : ""}"><title>${esc(tip)}</title></path>${labels ? `<text x="${xx}" y="${bottom+14}" text-anchor="middle" class="trade-label ${direction}">${buy ? "+" : "−"}${esc(fmt(r.lots))}</text>` : ""}</g>`;
    addMarker(r.index,tip);
  }
  for(const r of unpriced){
    const xx=x(r.index),buy=/buy/i.test(r.side);
    const tip=`${datetime(r.time)} · ${buy ? "Покупка" : "Продажа"}: ${fmt(r.lots)} лот. · цена исполнения ещё не получена (время сверки заявки)`;
    content+=`<g class="trade-mark" data-trade="${esc(r.key)}"><line x1="${xx}" x2="${xx}" y1="${top}" y2="${bottom}" class="trade-unpriced"/>${r.key===highlightedTrade ? `<circle cx="${xx}" cy="${top+8}" r="13" class="trade-highlight"/>` : ""}<circle cx="${xx}" cy="${top+8}" r="6" class="trade-unpriced-dot ${buy ? "buy" : "sell"}"><title>${esc(tip)}</title></circle></g>`;
    addMarker(r.index,tip);
  }
  for(const p of plans){
    const index=candleIndexAt(p.created_at);if(Date.parse(p.created_at)<seriesStart || index<start || index>=end)continue;
    const xx=x(index),yy=p.limit_price==null ? bottom+26 : y(Number(p.limit_price));
    const tip=`План: ${/buy/i.test(p.side) ? "покупка" : "продажа"} ${fmt(p.lots)} лот. · ${p.limit_price==null ? "по рынку, цена неизвестна" : fmt(p.limit_price,2)+" ₽"}`;
    if(p.limit_price!=null)content+=`<line x1="${left}" x2="${right}" y1="${yy}" y2="${yy}" class="plan-line"/>`;
    content+=`<path d="M ${xx} ${yy-6} L ${xx+6} ${yy} L ${xx} ${yy+6} L ${xx-6} ${yy} Z" class="plan-marker"><title>${esc(tip)}</title></path>`;
    addMarker(index,tip);
  }
  // Crosshair overlay; moved on pointer events without redrawing the candles.
  content+=`<g id="chart-crosshair" class="crosshair" visibility="hidden"><line id="cross-v" class="cross-line"/><line id="cross-h" class="cross-line"/><rect id="cross-price-box" class="cross-box" width="70" height="18" rx="3"/><text id="cross-price" class="cross-text"></text><rect id="cross-time-box" class="cross-box" height="18" rx="3"/><text id="cross-time" class="cross-text" text-anchor="middle"></text></g>`;
  svg.innerHTML=content;
  chartGeometry={left,right,top,bottom,axis,step,min,max,start,count};
  $("chart-range").textContent=`${fmt(count)} из ${fmt(chartCandles.length)} свечей · ${pointerTime(visibleCandles[0].time)} – ${pointerTime(visibleCandles.at(-1).time)} МСК${chartView.follow ? "" : " · прокручено назад"}`;
  document.querySelector('[data-chart="back"]').disabled=start===0;
  document.querySelector('[data-chart="forward"]').disabled=document.querySelector('[data-chart="latest"]').disabled=end===chartCandles.length;
  document.querySelector('[data-chart="zoom-in"]').disabled=count<=Math.min(MIN_VISIBLE,chartCandles.length);
  document.querySelector('[data-chart="zoom-out"]').disabled=count>=Math.min(MAX_VISIBLE,chartCandles.length);
  if(chartPointer && !chartDrag)drawCrosshair(chartPointer);else renderOhlc(end-1);
  renderTradeLegend(trades,inSeries.length,inView.length,unpriced.length,plans);
}

// Open/high/low/close line above the chart for the hovered (or the last visible) candle.
function renderOhlc(index) {
  const c=chartCandles[index];if(!c){$("asset-ohlc").textContent="";return;}
  const change=c.open ? (c.close-c.open)/c.open*100 : 0, direction=c.close>=c.open ? "up" : "down";
  const part=(name,value)=>`<span>${name} <b class="${direction}">${esc(value)}</b></span>`;
  $("asset-ohlc").innerHTML=`<span class="ohlc-time">${esc(pointerTime(c.time))} МСК${c.complete ? "" : " · формируется"}</span>${part("Откр",priceText(c.open))}${part("Макс",priceText(c.high))}${part("Мин",priceText(c.low))}${part("Закр",priceText(c.close))}${part("Изм",(change>0 ? "+" : "")+fmt(change,2)+"%")}${part("Объём",fmt(c.volume))}${(assetMarkers.get(index) || []).map(text=>`<span class="ohlc-trade">${esc(text)}</span>`).join("")}`;
}

function svgPoint(event) {
  const svg=$("asset-chart"), point=svg.createSVGPoint();
  point.x=event.clientX;point.y=event.clientY;
  return point.matrixTransform(svg.getScreenCTM().inverse());
}

function drawCrosshair(point) {
  const g=chartGeometry, cross=$("chart-crosshair");if(!g || !cross)return;
  if(point.x<g.left || point.x>g.right || point.y<g.top || point.y>g.bottom){cross.setAttribute("visibility","hidden");renderOhlc(g.start+g.count-1);return;}
  const offset=clamp(Math.floor((point.x-g.left)/g.step),0,g.count-1), index=g.start+offset, xx=g.left+(offset+.5)*g.step;
  const price=g.max-(point.y-g.top)/(g.bottom-g.top)*(g.max-g.min), label=pointerTime(chartCandles[index].time), width=label.length*6.4+16;
  const set=(id,values)=>{const element=$(id);for(const [key,value] of Object.entries(values))element.setAttribute(key,value);};
  set("cross-v",{x1:xx,x2:xx,y1:g.top,y2:g.bottom});
  set("cross-h",{x1:g.left,x2:g.right,y1:point.y,y2:point.y});
  set("cross-price-box",{x:g.right+2,y:point.y-9});
  set("cross-price",{x:g.right+8,y:point.y+4});$("cross-price").textContent=priceText(price);
  set("cross-time-box",{x:clamp(xx-width/2,g.left,g.right-width),y:g.axis+3,width});
  set("cross-time",{x:clamp(xx,g.left+width/2,g.right-width/2),y:g.axis+16});$("cross-time").textContent=label;
  cross.setAttribute("visibility","visible");
  renderOhlc(index);
}

function renderTradeLegend(trades,inSeries,inView,unpriced,plans) {
  const outside=trades.length-inSeries, offscreen=inSeries-inView;
  $("trade-summary").textContent=!trades.length ? "Сделок по активу пока нет — отметки появятся после первого исполнения" : `На графике ${fmt(inView)} из ${fmt(trades.length)} сделок${offscreen ? ` · ещё ${fmt(offscreen)} за краем экрана — прокрутите график` : ""}${outside ? ` · ${fmt(outside)} вне периода свечей — нажмите «Показать на графике» под сделкой` : ""}${unpriced ? ` · без цены: ${fmt(unpriced)}` : ""}`;
  const market=plans.some(p=>p.limit_price==null), limit=plans.some(p=>p.limit_price!=null);
  $("plan-legend").hidden=!plans.length;
  $("plan-legend").textContent=`◇ План: ${[limit ? "лимитная заявка — пунктир на уровне цены" : "",market ? "рыночная — под графиком, цена известна только после исполнения" : ""].filter(Boolean).join("; ")}`;
}

$("strategy-rows").addEventListener("click",event=>{const button=event.target.closest("[data-strategy]");if(button)chooseStrategy(button.dataset.strategy);});
$("close-detail").addEventListener("click",()=>chooseStrategy(""));
$("asset-select").addEventListener("change",()=>{highlightedTrade=null;renderDetailTrades();loadMarket(true);});
$("executed-trades").addEventListener("click",event=>{const button=event.target.closest("[data-show-trade]");if(button){showTrade(button.dataset.showTrade);renderDetailTrades();}});
$("asset-interval").addEventListener("click",event=>{const button=event.target.closest("[data-interval]");if(!button || button.dataset.interval===chartInterval())return;setChartInterval(button.dataset.interval);loadMarket(true);});
setChartInterval(chartInterval());
document.querySelector(".chart-nav").addEventListener("click",event=>{
  const action=event.target.closest("[data-chart]")?.dataset.chart;if(!action || !chartCandles.length)return;
  const half=Math.max(1,Math.round(chartView.count/2));
  if(action==="back")panBy(-half);
  if(action==="forward")panBy(half);
  if(action==="latest")moveView(chartCandles.length);
  if(action==="zoom-in")zoomBy(1/1.4);
  if(action==="zoom-out")zoomBy(1.4);
});
const chart=$("asset-chart");
chart.addEventListener("pointerdown",event=>{
  if(event.button!==0 || !chartGeometry)return;
  chartDrag={x:svgPoint(event).x,end:chartView.end};
  try{chart.setPointerCapture(event.pointerId);}catch(_){/* synthetic or already released pointer */}
  chart.classList.add("dragging");
  $("chart-crosshair")?.setAttribute("visibility","hidden");
});
chart.addEventListener("pointermove",event=>{
  if(!chartGeometry)return;
  const point=svgPoint(event);chartPointer=point;
  if(chartDrag){
    // Dragging right shows older candles, as in trading terminals.
    const shift=Math.round((point.x-chartDrag.x)/chartGeometry.step);
    if(chartDrag.end-shift!==chartView.end)moveView(chartDrag.end-shift);
    return;
  }
  drawCrosshair(point);
});
const endDrag=()=>{chartDrag=null;chart.classList.remove("dragging");if(chartPointer)drawCrosshair(chartPointer);};
chart.addEventListener("pointerup",endDrag);
chart.addEventListener("pointercancel",endDrag);
chart.addEventListener("pointerleave",()=>{if(chartDrag)return;chartPointer=null;$("chart-crosshair")?.setAttribute("visibility","hidden");if(chartGeometry)renderOhlc(chartGeometry.start+chartGeometry.count-1);});
chart.addEventListener("wheel",event=>{
  if(!chartGeometry)return;
  event.preventDefault();
  if(Math.abs(event.deltaX)>Math.abs(event.deltaY) || event.shiftKey){const delta=event.shiftKey ? event.deltaY || event.deltaX : event.deltaX;panBy(Math.sign(delta)*Math.max(1,Math.round(chartView.count*.08)));return;}
  const point=svgPoint(event), anchor=chartGeometry.start+clamp(Math.floor((point.x-chartGeometry.left)/chartGeometry.step),0,chartGeometry.count-1);
  zoomBy(event.deltaY>0 ? 1.15 : 1/1.15,anchor);
},{passive:false});
chart.addEventListener("keydown",event=>{
  if(!chartCandles.length)return;
  const actions={ArrowLeft:()=>panBy(event.shiftKey ? -Math.round(chartView.count/4) : -1),ArrowRight:()=>panBy(event.shiftKey ? Math.round(chartView.count/4) : 1),Home:()=>moveView(chartView.count),End:()=>moveView(chartCandles.length),"+":()=>zoomBy(1/1.2),"=":()=>zoomBy(1/1.2),"-":()=>zoomBy(1.2),"_":()=>zoomBy(1.2)};
  if(actions[event.key]){event.preventDefault();actions[event.key]();}
});
// Re-render in real pixels when the chart box changes size.
let resizeFrame=0;
new ResizeObserver(()=>{cancelAnimationFrame(resizeFrame);resizeFrame=requestAnimationFrame(()=>{if(chartCandles.length)renderAssetChart();});}).observe(document.querySelector(".asset-chart-wrap"));
