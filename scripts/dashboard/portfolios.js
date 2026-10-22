"use strict";

// Portfolios (#portfolios): what runs where. One card per portfolio: the account
// and its contour, state, value and result, every block with its strategies (an
// AI block shows the profile and the model) and instruments with their types.
// Old scenarios that gave their accounts to newer portfolios go to the archive.
let portfoliosArchive=false;
const INSTRUMENT_KINDS={share:"Акция",etf:"Фонд",bond:"Облигация",futures:"Фьючерс",currency:"Валюта",option:"Опцион",sp:"Структурный продукт",index:"Индекс",commodity:"Товар"};

$("strategies").insertAdjacentHTML("beforebegin", `<section class="panel" id="portfolios" data-view="portfolios"><div class="section-heading"><div><h2>Портфели <span id="portfolios-total" class="count-pill">0</span></h2><p class="muted">Что на каком счёте работает: стратегии, модели ИИ и инструменты каждого блока</p></div><button type="button" class="button" id="portfolios-archive"></button></div><div id="portfolio-cards" class="portfolio-grid"></div></section>`);

// A portfolio is current while it owns its account, runs, or is on a real account.
const isCurrentCase=c=>c.contour==="production" || c.owns_account || c.running;

function instrumentInfo(uid) {
  return (controlsData?.catalog || []).find(a=>a.uid===uid);
}

function modelOf(strategy) {
  const models=controlsData?.ai_models, name=strategy.params?.model_profile || models?.default;
  const profile=(models?.models || []).find(m=>m.name===name);
  return {profile:name || "по умолчанию",model:profile?.model};
}

function strategyLine(strategy, total) {
  const share=total>0 ? ` · ${fmt(Number(strategy.weight || 1)/total*100,0)}% блока` : "";
  if(strategy.kind==="ai-model"){
    const {profile,model}=modelOf(strategy);
    return `<span class="portfolio-strategy ai">ИИ · ${esc(profile)}${model ? ` <span class="muted-text">(${esc(model)})</span>` : ""}${share}</span>`;
  }
  return `<span class="portfolio-strategy">${esc(kindName(strategy.kind))}${share}</span>`;
}

function instrumentChip(asset) {
  const info=instrumentInfo(asset.uid), kind=info?.kind && info.kind!=="share" ? INSTRUMENT_KINDS[info.kind] || info.kind : "";
  return `<span class="instrument-chip" title="${esc(info?.name || asset.ticker)}">${esc(asset.ticker || asset.uid.slice(0,8))}${kind ? `<small>${esc(kind)}</small>` : ""}</span>`;
}

function lastResult(c) {
  const runs=(controlsData?.ranking?.history || []).filter(r=>r.strategy_id===c.strategy_id);
  return runs[0];
}

function lastActivity(c) {
  const orders=data.orders.filter(o=>o.strategy_id===c.strategy_id).sort((a,b)=>Date.parse(b.updated_at || b.created_at)-Date.parse(a.updated_at || a.created_at));
  return orders[0] ? `Последняя заявка: ${datetime(orders[0].updated_at || orders[0].created_at)}` : "Заявок ещё не было";
}

function portfolioCard(c) {
  const account=data.accounts.find(a=>a.id===c.account_id), [state,type]=caseState(c), result=lastResult(c);
  const blocks=(c.blocks || []).map(b=>{
    const total=(b.strategies || []).reduce((sum,s)=>sum+Number(s.weight || 1),0);
    return `<div class="pf-block"><div class="block-share">${fmt(b.share,0)}%</div><div class="block-body"><div class="block-strategies">${(b.strategies || []).map(s=>strategyLine(s,(b.strategies || []).length>1 ? total : 0)).join("")}</div><div class="block-instruments">${(b.instruments || []).map(instrumentChip).join("")}</div></div></div>`;
  }).join("") || `<div class="pf-block"><div class="block-share">100%</div><div class="block-body"><div class="block-strategies"><span class="portfolio-strategy">${esc(kindName(c.kind))}</span></div><div class="block-instruments">${(c.assets || []).filter(a=>!a.benchmark).map(instrumentChip).join("") || '<span class="muted-text">Состав не записан</span>'}</div></div></div>`;
  const positions=account?.positions || [];
  return `<article class="portfolio-card${c.contour==="production" ? " live" : ""}">
    <div class="portfolio-head"><div><a class="portfolio-name" href="#strategy/${encodeURIComponent(c.kind)}">${esc(displayName(c))}</a><span class="subtext">${esc(c.kind)} · счёт ${esc(c.account_id.slice(0,8))}</span></div><div class="portfolio-badges">${c.contour==="production" ? badge("Реальный счёт","live") : badge("Песочница","sandbox")}${badge(state,type)}</div></div>
    <dl class="portfolio-facts"><div><dt>Стоимость счёта</dt><dd>${account?.equity!=null ? fmt(account.equity) + " ₽" : "—"}</dd></div><div><dt>Результат</dt><dd>${result?.return_percent!=null ? `${result.return_percent>0 ? "+" : ""}${fmt(result.return_percent,2)}%` : "—"}</dd></div><div><dt>Позиции</dt><dd title="${esc(positions.map(p=>p.instrument).join(", "))}">${positions.length}</dd></div><div><dt>Расписание</dt><dd>${esc(scheduleSummary(c))}</dd></div></dl>
    <div class="pf-blocks">${blocks}</div>
    <div class="portfolio-foot"><span>${esc(lastActivity(c))}</span><a href="#strategy/${encodeURIComponent(c.kind)}">Открыть →</a></div>
  </article>`;
}

function renderPortfolios() {
  if(!data)return;
  const current=data.cases.filter(isCurrentCase), archived=data.cases.filter(c=>!isCurrentCase(c));
  const shown=portfoliosArchive ? archived : current;
  $("portfolios-total").textContent=current.length;
  $("portfolios-archive").textContent=portfoliosArchive ? "← Текущие портфели" : `Архив (${archived.length})`;
  $("portfolios-archive").hidden=!archived.length && !portfoliosArchive;
  const html=shown.length ? shown.map(portfolioCard).join("") : empty(portfoliosArchive ? "Архив пуст" : "Портфелей пока нет","Портфель создаётся в «Управление стендом → Новый портфель».");
  if($("portfolio-cards").dataset.html!==html){$("portfolio-cards").innerHTML=html;$("portfolio-cards").dataset.html=html;}
}

$("portfolios-archive").addEventListener("click",()=>{portfoliosArchive=!portfoliosArchive;renderPortfolios();});
