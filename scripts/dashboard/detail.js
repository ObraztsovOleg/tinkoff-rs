"use strict";

let detailKind = null, marketSeries = null, marketKey = "", marketRequest = 0, marketFetched = 0;
let visibleCandles = [], assetMarkers = [];
const pendingStates = new Set(["pending", "submitting", "accepted", "uncertain"]);
const detailStatuses = {pending:"В плане",submitting:"Отправляется",accepted:"У брокера",uncertain:"Проверка исполнения",filled:"Исполнена",cancelled:"Отменена",rejected:"Отклонена"};

function chooseStrategy(kind) {
  $("strategy-filter").value = kind;
  renderStrategies();renderActivity();renderAccounts();renderDetail();
  if(kind)$("strategy-detail").scrollIntoView({behavior:"smooth",block:"start"});
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

function renderDetail() {
  const c=selectedCase();
  $("strategy-detail").hidden=!c;
  if(!c){detailKind=null;marketRequest++;return;}
  const changed=detailKind!==c.kind;
  detailKind=c.kind;
  $("detail-name").textContent=c.kind;
  $("detail-schedule").textContent=c.running ? "Процесс запущен · сигналы появляются по рыночным событиям" : c.state === "cleaned" ? "Сценарий завершён · сохранена история сделок" : `Прогрев и запуск: ${datetime(c.launch_at)} МСК · открытие: ${datetime(c.opening_at)} МСК${c.predecessor && !c.owns_account ? " · перед запуском ожидается освобождение счёта" : ""}`;
  const old=$("asset-select").value;
  const chartAssets=typeof allAssets === "function" ? allAssets(c) : c.assets || [];
  $("asset-select").innerHTML=chartAssets.map(a=>`<option value="${esc(a.uid)}">${esc(a.ticker)}${a.benchmark ? " · ориентир рынка" : ""}</option>`).join("");
  if(!changed && chartAssets.some(a=>a.uid===old))$("asset-select").value=old;
  if(changed)$("asset-interval").value=c.kind.includes("intraday") ? "30m" : "day";
  renderDetailTrades();loadMarket();
  if(typeof renderControls === "function")renderControls();
}

function renderDetailTrades() {
  const c=selectedCase();if(!c)return;
  const fills=executedFills(), filledRequests=new Set(fills.map(f=>f.request_id));
  const executed=detailOrders().filter(o => !filledRequests.has(o.request_id) && (o.status === "filled" || Number(o.lots_executed)>0));
  $("executed-count").textContent=fills.length+executed.length;
  const rows=fills.map(f=>({time:f.executed_at,side:f.side,lots:f.lots,price:f.price,maintenance:f.maintenance,manual:f.manual,confirmed:true})).concat(executed.map(o=>({time:o.updated_at,side:o.side,lots:o.lots_executed,price:null,maintenance:o.maintenance,manual:o.manual,confirmed:false}))).sort((a,b)=>String(b.time).localeCompare(a.time));
  $("executed-trades").innerHTML=rows.length ? table(["Время, МСК","Сделка","Лоты","Цена"],rows.map(r=>`<tr><td>${esc(datetime(r.time))}${!r.confirmed ? '<span class="subtext">Время сверки заявки</span>' : ""}</td><td>${side(r.side)}${r.maintenance ? '<span class="subtext warning">Служебное закрытие</span>' : r.manual ? '<span class="subtext">Ручная сделка</span>' : ""}</td><td>${fmt(r.lots)}</td><td>${r.price==null ? '<span class="muted-text">Не получена</span>' : fmt(r.price,2)}</td></tr>`)) : empty("Сделок по этому активу ещё нет","После исполнения здесь появятся лоты и подтверждённая цена.");
  const plans=plannedOrders();
  $("planned-count").textContent=plans.length;
  $("planned-trades").innerHTML=plans.length ? table(["Направление","Лоты","Цена","Состояние"],plans.map(o=>`<tr><td>${side(o.side)}${o.maintenance ? '<span class="subtext warning">Служебное закрытие</span>' : ""}</td><td>${fmt(o.lots)}<span class="subtext">Цель: ${fmt(o.target_lots)} лот.</span></td><td>${o.limit_price==null ? "По рынку" : fmt(o.limit_price,2)}</td><td>${badge(detailStatuses[o.status] || o.status,o.status === "uncertain" ? "warn" : "waiting")}</td></tr>`)) : empty("Запланированных заявок нет","Стратегия ещё не сохранила актуальную заявку по этому активу.");
  const latest=(data.targets || []).filter(t=>t.strategy===c.kind && !t.maintenance).sort((a,b)=>String(b.generated_at).localeCompare(a.generated_at))[0];
  const weight=latest?.weights.find(w=>w.instrument_id===$("asset-select").value);
  $("target-summary").textContent=weight ? `Последняя цель: ${fmt(weight.weight*100,2)}% портфеля · ${datetime(latest.generated_at)} МСК${Date.parse(latest.valid_until) <= Date.now() ? " · срок исполнения истёк" : ""}. Целевая доля не является обещанием сделки.` : "Будущий сигнал неизвестен. Здесь отображаются только сохранённые планы, без прогнозирования новых сделок.";
}

async function loadMarket(force=false) {
  const uid=$("asset-select").value,interval=$("asset-interval").value,key=`${uid}:${interval}`;
  if(!uid){$("asset-empty").hidden=false;$("asset-empty").textContent="Для стратегии пока не определены инструменты.";return;}
  if(!force && key===marketKey && Date.now()-marketFetched<5000){renderAssetChart();return;}
  const changed=key!==marketKey;
  marketKey=key;marketFetched=Date.now();
  if(changed){marketSeries=null;$("asset-chart").replaceChildren();$("asset-empty").hidden=false;$("asset-empty").textContent="Загружаем свечи инструмента…";$("asset-price").textContent="—";$("asset-time").textContent="";}
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

function renderAssetChart() {
  if(!marketSeries || !selectedCase())return;
  visibleCandles=marketSeries.candles.filter(c=>[c.open,c.high,c.low,c.close].every(Number.isFinite) && Number.isFinite(Date.parse(c.time)));
  const svg=$("asset-chart");$("asset-tooltip").hidden=true;
  $("asset-empty").hidden=visibleCandles.length>0;
  if(!visibleCandles.length){svg.replaceChildren();$("asset-empty").textContent=marketSeries.loading ? "История загружается…" : marketSeries.retry_at ? "Источник ограничил запросы; повторим загрузку автоматически." : "Свечей в выбранном периоде нет.";return;}
  const last=visibleCandles.at(-1),asset=(typeof allAssets === "function" ? allAssets() : selectedCase().assets).find(a=>a.uid===$("asset-select").value);
  $("asset-price").textContent=`${fmt(last.close,2)} ₽`;
  $("asset-time").textContent=`${asset?.ticker || ""} · ${datetime(last.time)} МСК${last.complete ? " · закрытие свечи" : " · свеча формируется"}`;
  $("asset-freshness").textContent=`Котировки: ${ageText(marketSeries.observed_at)}${marketSeries.error || age(marketSeries.observed_at)>180 ? " · обновление задерживается" : ""}`;
  const intervalMilliseconds={"5s":5000,"10s":10000,"30s":30000,"5m":300000,"30m":1800000,day:86400000};
  const firstTime=Date.parse(visibleCandles[0].time),endTime=Date.parse(last.time)+intervalMilliseconds[marketSeries.interval];
  const fills=executedFills().filter(f=>Number(f.price)>0 && Number.isFinite(Number(f.price)) && Date.parse(f.executed_at)>=firstTime && Date.parse(f.executed_at)<=endTime);
  const plans=plannedOrders();
  const levels=plans.map(o=>Number(o.limit_price)).filter(p=>p>0);
  const lows=visibleCandles.map(c=>c.low).concat(fills.map(f=>Number(f.price)),levels),highs=visibleCandles.map(c=>c.high).concat(fills.map(f=>Number(f.price)),levels);
  const low=Math.min(...lows),high=Math.max(...highs),padding=Math.max((high-low)*.08,high*.001,.01),min=low-padding,max=high+padding;
  const y=price=>246-(price-min)/(max-min)*218,step=880/visibleCandles.length,x=i=>85+(i+.5)*step;
  // Equal bar spacing avoids compressing candles around overnight/weekend gaps.
  const indexAt=time=>{const stamp=Date.parse(time);let index=0;for(let i=0;i<visibleCandles.length;i++){if(Date.parse(visibleCandles[i].time)>stamp)break;index=i;}return index;};
  let content="";for(let i=0;i<5;i++){const yy=28+i*218/4;content+=`<line x1="85" x2="965" y1="${yy}" y2="${yy}" class="chart-grid"/><text x="73" y="${yy+4}" text-anchor="end" class="chart-label">${esc(fmt(max-i*(max-min)/4,2))}</text>`;}
  const width=Math.max(.8,Math.min(12,step*.65));
  content+=visibleCandles.map((c,i)=>{const xx=x(i),cls=`${c.close>=c.open ? "candle-up" : "candle-down"}${c.complete ? "" : " forming"}`;return `<line x1="${xx}" x2="${xx}" y1="${y(c.high)}" y2="${y(c.low)}" class="${cls}"/><rect x="${xx-width/2}" y="${Math.min(y(c.open),y(c.close))}" width="${width}" height="${Math.max(.8,Math.abs(y(c.open)-y(c.close)))}" class="${cls}"/>`;}).join("");
  assetMarkers=[];
  for(const f of fills){const xx=x(indexAt(f.executed_at)),yy=y(Number(f.price)),buy=/buy/i.test(f.side),delta=buy ? 1 : -1;const tip=`${datetime(f.executed_at)} · ${buy ? "Покупка" : "Продажа"}: ${fmt(f.lots)} лот. × ${fmt(f.price,2)} ₽${f.maintenance ? " · служебное закрытие" : f.manual ? " · ручная сделка" : ""}`;content+=`<path d="M ${xx} ${yy-7*delta} L ${xx-6} ${yy+5*delta} L ${xx+6} ${yy+5*delta} Z" class="${buy ? "trade-buy" : "trade-sell"}${f.maintenance ? " trade-maintenance" : f.manual ? " trade-manual" : ""}"><title>${esc(tip)}</title></path>`;assetMarkers.push({index:indexAt(f.executed_at),text:tip});}
  for(const p of plans){const t=Date.parse(p.created_at);if(t<firstTime || t>endTime)continue;const i=indexAt(p.created_at),xx=x(i),yy=p.limit_price==null ? 268 : y(Number(p.limit_price));const tip=`План: ${/buy/i.test(p.side) ? "покупка" : "продажа"} ${fmt(p.lots)} лот. · ${p.limit_price==null ? "по рынку, цена неизвестна" : fmt(p.limit_price,2)+" ₽"}`;if(p.limit_price!=null)content+=`<line x1="85" x2="965" y1="${yy}" y2="${yy}" class="plan-line"/>`;content+=`<path d="M ${xx} ${yy-6} L ${xx+6} ${yy} L ${xx} ${yy+6} L ${xx-6} ${yy} Z" class="plan-marker"><title>${esc(tip)}</title></path>`;assetMarkers.push({index:i,text:tip});}
  const label = time => marketSeries.interval === "day" ? date(time,{year:"numeric"}) : datetime(time);
  content+=`<text x="85" y="302" class="chart-label">${esc(label(visibleCandles[0].time))}</text><text x="965" y="302" text-anchor="end" class="chart-label">${esc(label(last.time))} МСК</text>`;
  svg.innerHTML=content;
}

$("strategy-rows").addEventListener("click",event=>{const button=event.target.closest("[data-strategy]");if(button)chooseStrategy(button.dataset.strategy);});
$("close-detail").addEventListener("click",()=>chooseStrategy(""));
$("asset-select").addEventListener("change",()=>{renderDetailTrades();loadMarket(true);});
$("asset-interval").addEventListener("change",()=>loadMarket(true));
$("asset-chart").addEventListener("pointermove",event=>{
  if(!visibleCandles.length || !marketSeries)return;
  const xx=svgX(event);
  const index=Math.max(0,Math.min(visibleCandles.length-1,Math.floor((xx-85)/880*visibleCandles.length))),c=visibleCandles[index];
  $("asset-tooltip").textContent=`${datetime(c.time)} МСК · O ${fmt(c.open,2)} · H ${fmt(c.high,2)} · L ${fmt(c.low,2)} · C ${fmt(c.close,2)} · объём ${fmt(c.volume)}${c.complete ? "" : " · формируется"}${assetMarkers.filter(m=>m.index===index).map(m=>" | "+m.text).join("")}`;
  $("asset-tooltip").hidden=false;
});
$("asset-chart").addEventListener("pointerleave",()=>{$("asset-tooltip").hidden=true;});
