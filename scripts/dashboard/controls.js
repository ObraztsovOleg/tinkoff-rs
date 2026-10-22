"use strict";

let controlsData=null, controlsAt=0, controlsBusy=false, controlCase=null, csrf=null;
let scheduleDraft=null, portfolioDraft=null;
const commandNames={pause:"Выключить",resume:"Включить",manual_order:"Ручная заявка",cleanup:"Закрыть позиции",budget:"Бюджет",pay_in:"Пополнение",mode:"Режим стенда",execution_mode:"Режим заявок",cancel_batch:"Отменить план",approve_batch:"Подтвердить план",universe:"Состав инструментов",new_run:"Новый чистый прогон",catalog_add:"Добавить инструмент",schedule:"Расписание",runtime_limit:"Лимит процессов",trade_band:"Порог сделки",ai_budget:"Лимит расходов на ИИ",portfolio:"Сборка портфеля",portfolio_create:"Новый портфель",ai_check:"Проверка модели",ai_preview:"Пробный анализ"};
const commandStates={queued:"В очереди",running:"Выполняется",succeeded:"Выполнено",failed:"Отклонено",uncertain:"Исход требует сверки"};
// Builtin strategies and how many instruments each needs inside its block: [name, minimum, maximum].
const KINDS={"moving-average-trend":["Тренд по скользящим средним",1,null],"range-breakout":["Пробой диапазона",1,null],"time-series-momentum":["Моментум временного ряда",1,null],"cross-sectional-momentum":["Кросс-секционный моментум",10,null],"intermediate-momentum":["Промежуточный моментум",10,null],"fifty-two-week-high":["Близость к 52-недельному максимуму",10,null],"residual-momentum":["Остаточный моментум",10,null],"pairs-trading":["Парная торговля",2,null],"short-term-reversal":["Краткосрочный разворот",10,null],"market-intraday-momentum":["Внутридневной моментум рынка",1,1],"intraday-same-clock":["Внутридневной same-clock",10,null],"low-volatility":["Низкая волатильность",10,null],"betting-against-beta":["Против беты",10,null],"equal-risk-contribution":["Равный вклад в риск",2,null],"volatility-managed":["Управление волатильностью",4,null],"value-composite":["Value: дешёвые против дорогих",10,null],"quality-profitability":["Качество: рентабельность и низкий долг",10,null],"risk-managed-momentum":["Моментум с контролем риска",10,null],"return-seasonality":["Сезонность доходностей",10,null],"dividend-month":["Дивидендная премия",1,null],"ai-model":["ИИ-модель: новости и индикаторы",1,50]};
const kindName=kind=>KINDS[kind]?.[0] || kind;
// Strategies computed from company reports and dividends: shares only.
const SHARE_ONLY=new Set(["value-composite","quality-profitability","value-momentum","multi-factor","dividend-month"]);
const assetKind=uid=>(controlsData?.catalog || []).find(a=>a.uid===uid)?.kind || "share";
const kindNeed=kind=>{const [,low,high]=KINDS[kind] || [kind,1,null];return high===low ? `ровно ${low}` : `от ${low}`;};

document.querySelector(".chart-panel").insertAdjacentHTML("afterend", `<section class="panel control-panel" id="control-overview"><div class="section-heading"><div><h2>Управление стендом</h2><p id="control-connection" class="muted">Подключаем управление…</p></div><form id="limit-form" class="inline-form"><label for="runtime-limit">Процессов одновременно</label><input id="runtime-limit" type="number" min="1" max="10" step="1" required><button class="button" type="submit">Сохранить</button></form></div></section>`);
$("strategy-detail").querySelector(".asset-toolbar").insertAdjacentHTML("beforebegin", `<div class="strategy-controls"><div class="control-actions"><button class="button primary" data-command="resume">Включить</button><button class="button" data-command="pause">Выключить</button><button class="button danger" data-command="cleanup">Закрыть позиции</button><button class="button" data-command="new_run">Новый чистый прогон</button><form id="execution-form" class="inline-form"><label for="execution-mode">Планы</label><select id="execution-mode"><option value="auto">Авто</option><option value="confirm">Подтверждать</option></select><button class="button" type="submit">Сохранить</button></form></div><div id="control-case-state" class="detail-note"></div><div id="control-balances" class="control-balances"></div><form id="schedule-form" class="control-form schedule-form"><h3>Когда торговать</h3><div class="run-mode" role="radiogroup" aria-label="Режим торговли"><label><input type="radio" name="run-mode" value="schedule"> По выбранным дням</label><label><input type="radio" name="run-mode" value="always"> Постоянно, пока включена</label></div><div id="schedule-days" class="day-picker" aria-label="Торговые дни MOEX"></div><label class="log-check"><input id="schedule-close" type="checkbox"> Закрывать позиции после каждого окна</label><p id="schedule-preview" class="detail-note"></p><button class="button primary" type="submit">Сохранить расписание</button></form><div class="control-grid"><form id="manual-form" class="control-form"><h3>Своя заявка</h3><label>Инструмент торговли<select id="manual-instrument" required></select></label><div class="form-pair"><label>Направление<select id="manual-side"><option value="buy">Покупка</option><option value="sell">Продажа</option></select></label><label>Лоты<input id="manual-lots" type="number" min="1" step="1" value="1" required></label></div><div class="form-pair"><label>Тип<select id="manual-type"><option value="market">Рыночная</option><option value="limit">Лимитная</option></select></label><label>Цена, ₽<input id="manual-price" type="number" min="0.000000001" step="any" disabled></label></div><p class="detail-note">Автоторговля будет выключена до вашего включения. Лоты, бюджет и риск проверяются перед отправкой.</p><button type="submit" class="button primary">Отправить ручную заявку</button></form><div><form id="budget-form" class="control-form"><h3>Торговый бюджет</h3><p class="detail-note">Сейчас: <strong id="budget-current">—</strong></p><label>Новый бюджет, ₽<input id="budget-amount" type="number" min="0.01" step="0.01" required></label><p id="budget-delta" class="detail-note">Введите сумму — разницу стенд посчитает сам.</p><button class="button" type="submit">Установить</button></form><form id="payin-form" class="control-form"><h3>Пополнение sandbox-счёта</h3><label>Сумма, ₽<input id="payin-amount" type="number" min="0.01" max="30000000" step="0.01" required></label><button class="button" type="submit">Пополнить</button><p class="detail-note">Пополнение не меняет бюджет и не считается прибылью. API не позволяет вычитать деньги со счёта.</p></form></div></div><div class="budget-history"><h3>Журнал бюджета <span id="budget-history-count" class="count-pill">0</span></h3><div id="budget-history"></div></div><div id="approval-plans"></div><details class="universe-editor" id="portfolio-editor"><summary>Собрать портфель счёта</summary><form id="portfolio-form"><label>Название портфеля<input id="portfolio-title" maxlength="60" required></label><div id="portfolio-blocks"></div><button type="button" class="button" id="portfolio-add-block">+ Добавить блок</button><p id="portfolio-check" class="detail-note" role="status"></p><p class="detail-note">Блок — одна стратегия или комбинация нескольких стратегий на общем наборе инструментов; их сигналы складываются с весами. Наборы разных блоков на одном счёте не пересекаются, одинаковые наборы — только на разных счетах. Доли блоков делят капитал счёта и в сумме дают 100%. Перед сборкой стратегия будет выключена; нужны закрытые позиции и завершённые заявки. Создаётся новая ревизия с прогревом, расписание и бюджет сохраняются.</p><div class="control-actions"><button class="button primary" type="submit">Создать ревизию и прогреть</button><button class="button" type="button" id="portfolio-create">Создать новым портфелем на этом счёте</button></div><p class="detail-note">«Новым портфелем» — текущая стратегия остаётся в списке со своей историей, а счёт переходит к новому портфелю. Все стратегии счёта должны быть выключены.</p></form></details></div>`);
$("activity").insertAdjacentHTML("beforebegin", `<section class="panel" id="ranking"><div class="section-heading"><div><h2>Рейтинг стратегий</h2><p class="muted">Доходность относительно выделенного капитала · после комиссий · без пополнений</p></div><label>Выборка<select id="ranking-filter"><option value="clean">Автоматические прогоны</option><option value="mixed">С ручным управлением</option><option value="history">История прогонов</option><option value="unranked">Без места в рейтинге</option></select></label></div><div id="ranking-content"></div><div id="ranking-unranked" class="panel-footnote"></div></section><section class="panel" id="instrument-catalog"><div class="section-heading"><div><h2>Инструменты</h2><p class="muted">Весь каталог T-Invest: акции, фонды, облигации, фьючерсы, валюта · добавление в список не меняет состав стратегии</p></div></div><form id="search-form" class="inline-form catalog-search"><label for="instrument-query">Тикер или название</label><input id="instrument-query" placeholder="Например, SBER, BR, ОФЗ, Si" maxlength="80" required><label for="instrument-kind" class="sr-only">Тип</label><select id="instrument-kind"><option value="">Все типы</option><option value="share">Акции</option><option value="etf">Фонды</option><option value="bond">Облигации</option><option value="futures">Фьючерсы</option><option value="currency">Валюта</option></select><button class="button" type="submit">Найти</button></form><p class="detail-note">Фьючерс лучше добавлять «непрерывным»: стенд торгует ближайший контракт и за 5 дней до последнего дня торгов сам переходит на следующий. Стратегии на отчётности и дивидендах работают только с акциями.</p><div id="instrument-results"></div><div id="catalog-list" class="catalog-list"></div></section><section class="panel" id="journal"><div class="section-heading"><div><h2>Журнал управления</h2><p class="muted">Каждая команда и её подтверждённый результат</p></div></div><div id="command-history"></div></section>`);

const signed=value=>{const n=Number(value);return !Number.isFinite(n) ? "—" : (n>0 ? "+" : "")+fmt(n,2);};
function commandResult(c) {
  const r=c.result || {};
  if(r.execution_budget!==undefined)return `Бюджет: ${fmt(r.execution_budget,2)} ₽${r.previous_budget!==undefined ? ` · было ${fmt(r.previous_budget,2)} ₽, изменение ${signed(r.delta)} ₽` : ""} · база риска сохранена`;
  if(r.message || r.error)return r.message || r.error;
  if(r.requires_confirmation)return "План изменился: требуется новое подтверждение";
  if(r.enabled!==undefined)return r.enabled ? "Автоторговля разрешена после проверки готовности" : "Автоторговля выключена; позиции сохранены";
  if(r.cancelled)return "План отменён, результат сверки сохранён";
  if(r.outcome)return `Результат исполнения: ${r.outcome}`;
  if(r.mode)return `Режим: ${r.mode==="confirm" ? "подтверждать" : r.mode==="auto" ? "авто" : r.mode}`;
  return c.status==="succeeded" ? "Изменение сохранено" : c.status==="uncertain" ? "Проверьте операции и остаток перед повторным действием" : "Ожидает владельца счёта и сверки";
}
function allAssets(c=selectedCase()) {
  const assets=new Map();
  for(const a of [...(data?.cases || []).flatMap(x=>x.assets || []),...(controlsData?.catalog || [])])assets.set(a.uid,a);
  for(const a of c?.assets || [])assets.set(a.uid,a);
  return [...assets.values()];
}
function controlFor(c) {return controlsData?.cases?.find(x=>x.strategy_id===c?.strategy_id);}
function selectAssets(element,assets,keep=true) {
  const prior=keep ? new Set([...element.selectedOptions].map(o=>o.value)) : new Set();
  const options=assets.map(a=>`<option value="${esc(a.uid)}">${esc(a.ticker || a.uid)}${a.benchmark ? " · benchmark" : ""}</option>`).join("");
  if(element.dataset.options===options && element.options.length===assets.length)return;
  element.innerHTML=options;element.dataset.options=options;
  for(const option of element.options)if(prior.has(option.value))option.selected=true;
}

// Same rule as the supervisor: consecutive chosen trading days form one window.
function previewWindows(days) {
  const chosen=new Set(days), windows=[];let block=[];
  for(const day of tradingSessions()){
    if(chosen.has(day.date))block.push(day);
    else if(block.length){windows.push(block);block=[];}
  }
  if(block.length)windows.push(block);
  return windows.map(b=>({days:b.map(d=>d.date),begin:Date.parse(b[0].startTime)-3600000,open:b[0].startTime,end:Date.parse(b.at(-1).endTime)+120000}));
}
function busyDays(c) {
  const days=new Map();
  for(const row of controlsData?.cases || []){
    if(row.account_id!==c.account_id || row.strategy_id===c.strategy_id || row.superseded_by || row.run_mode!=="schedule")continue;
    const name=data.cases.find(x=>x.strategy_id===row.strategy_id)?.kind || row.strategy_id;
    for(const day of row.trading_days || [])days.set(day,name);
  }
  return days;
}
function renderSchedule(c, control) {
  const key=`${c.strategy_id}:${JSON.stringify([control?.run_mode,control?.trading_days,control?.close_after])}`;
  if(!scheduleDraft || scheduleDraft.strategy!==c.strategy_id || (!scheduleDraft.dirty && scheduleDraft.key!==key))
    scheduleDraft={key,strategy:c.strategy_id,dirty:false,run_mode:control?.run_mode || "schedule",days:new Set(control?.trading_days || []),close_after:control?.close_after!==false};
  const draft=scheduleDraft, taken=busyDays(c), now=Date.now();
  for(const radio of document.querySelectorAll('[name="run-mode"]'))radio.checked=radio.value===draft.run_mode;
  $("schedule-close").checked=draft.close_after;
  $("schedule-close").disabled=draft.run_mode!=="schedule";
  const future=tradingSessions().filter(d=>Date.parse(d.endTime)>now).slice(0,25);
  $("schedule-days").innerHTML=future.length ? future.map(d=>{
    const owner=taken.get(d.date), pressed=draft.days.has(d.date);
    return `<button type="button" class="day-chip" data-day="${esc(d.date)}" aria-pressed="${pressed}" ${owner || draft.run_mode!=="schedule" ? "disabled" : ""} title="${esc(owner ? "Счёт в этот день торгует "+owner : `${time(d.startTime)}–${time(d.endTime)} МСК`)}">${esc(weekday(d.startTime))}${owner ? `<small>${esc(owner)}</small>` : ""}</button>`;
  }).join("") : '<span class="muted-text">Календарь торговых дней ещё не получен.</span>';
  const windows=previewWindows([...draft.days]).filter(w=>w.end>now);
  $("schedule-preview").textContent=draft.run_mode==="always" ? "Стратегия торгует, пока включена: «Выключить» останавливает торговлю. Дни не учитываются." : windows.length
    ? windows.map(w=>`${dayRange(w.days)}: прогрев с ${datetime(new Date(w.begin).toISOString())}, торговля ${datetime(w.open)} – ${datetime(new Date(w.end).toISOString())} МСК`).join(" · ")+(draft.close_after ? ". После каждого окна позиции закрываются." : ". После окна позиции остаются, процесс останавливается.")
    : "Дни не выбраны: стратегия не будет торговать по расписанию. Отметьте торговые дни или выберите «Постоянно».";
}

function renderBudget(c, control) {
  const current=control?.execution_budget ?? c.risk?.budget;
  $("budget-current").textContent=current==null ? "—" : `${fmt(current,2)} ₽`;
  const entered=Number($("budget-amount").value);
  $("budget-delta").textContent=$("budget-amount").value && Number.isFinite(entered) && current!=null ? `Изменение: ${signed(entered-Number(current))} ₽ (было ${fmt(current,2)} ₽)` : "Введите сумму — разницу стенд посчитает сам.";
  const history=(controlsData?.budget_history || []).filter(h=>h.account_id===c.account_id);
  $("budget-history-count").textContent=history.length;
  $("budget-history").innerHTML=history.length ? table(["Время, МСК","Было → стало, ₽","Изменение, ₽","Статус"],history.map(h=>{
    const r=h.result || {}, after=r.execution_budget ?? h.payload?.amount;
    return `<tr><td>${esc(datetime(h.created_at))}</td><td>${r.previous_budget!==undefined ? `${fmt(r.previous_budget,2)} → ` : ""}${fmt(after,2)}</td><td>${r.delta!==undefined ? signed(r.delta) : "—"}</td><td>${badge(commandStates[h.status] || h.status,h.status==="failed" ? "error" : h.status==="succeeded" ? "" : "waiting")}${h.status==="failed" ? `<span class="subtext warning">${esc(r.message || r.error || "Отклонено")}</span>` : ""}</td></tr>`;
  })) : empty("Изменений бюджета ещё не было","Каждая установка бюджета записывается сюда: прежняя сумма, новая и разница.");
}

// Portfolio builder state: blocks of strategies over disjoint instrument sets.
function defaultPortfolio(c) {
  if(c.blocks?.length)return {title:c.title || c.kind,blocks:c.blocks.map(b=>({share:String(b.share),instruments:b.instruments.map(i=>i.uid),strategies:b.strategies.map(s=>({kind:s.kind,weight:String(s.weight ?? "1"),...(s.params ? {params:{...s.params}} : {})}))}))};
  return {title:c.title || c.kind,blocks:[{share:"100",instruments:(c.assets || []).filter(a=>!a.benchmark).map(a=>a.uid),strategies:[{kind:KINDS[c.kind] ? c.kind : "moving-average-trend",weight:"1"}]}]};
}
function sameSet(left,right) {return left.length===right.length && left.every(x=>right.includes(x));}
function portfolioErrors(draft) {
  const errors=[], seen=new Map();let shares=0;
  if(!draft.title.trim())errors.push("Укажите название портфеля");
  if(!draft.blocks.length)errors.push("Добавьте хотя бы один блок");
  draft.blocks.forEach((b,index)=>{
    const n=index+1, share=Number(b.share);
    if(!(share>0 && share<=100))errors.push(`Блок ${n}: доля от 0 до 100%`);else shares+=share;
    if(!b.instruments.length)errors.push(`Блок ${n}: выберите инструменты`);
    if(!b.strategies.length)errors.push(`Блок ${n}: добавьте стратегию`);
    const kinds=b.strategies.map(s=>s.kind);
    if(new Set(kinds).size!==kinds.length)errors.push(`Блок ${n}: стратегия указана дважды`);
    for(const s of b.strategies){
      const [,low,high]=KINDS[s.kind] || [s.kind,1,null];
      if(b.instruments.length<low || (high!=null && b.instruments.length>high))errors.push(`Блок ${n}: ${kindName(s.kind)} требует ${kindNeed(s.kind)} инструментов`);
      if(!(Number(s.weight)>0))errors.push(`Блок ${n}: вес стратегии должен быть больше нуля`);
      if(SHARE_ONLY.has(s.kind) && b.instruments.some(uid=>assetKind(uid)!=="share"))errors.push(`Блок ${n}: ${kindName(s.kind)} работает только с акциями`);
    }
    for(const uid of b.instruments){
      if(seen.has(uid)){const other=seen.get(uid);errors.push(sameSet(draft.blocks[other-1].instruments,b.instruments) ? `Блоки ${other} и ${n} одинаковые — объедините их в одну комбинацию или разнесите по разным счетам` : `Блоки ${other} и ${n} пересекаются по инструментам`);break;}
      seen.set(uid,n);
    }
  });
  if(draft.blocks.length && Math.abs(shares-100)>1e-9)errors.push(`Доли блоков должны в сумме давать 100% (сейчас ${fmt(shares,2)}%)`);
  return [...new Set(errors)];
}
function renderPortfolio(c) {
  const source=JSON.stringify(c.blocks?.length ? c.blocks : c.assets);
  if(!portfolioDraft || portfolioDraft.strategy!==c.strategy_id || (!portfolioDraft.dirty && portfolioDraft.source!==source))
    portfolioDraft={...defaultPortfolio(c),strategy:c.strategy_id,source,dirty:false};
  const draft=portfolioDraft, assets=allAssets(c).filter(a=>!a.benchmark);
  if(document.activeElement!==$("portfolio-title"))$("portfolio-title").value=draft.title;
  const owner=new Map();draft.blocks.forEach((b,i)=>b.instruments.forEach(uid=>owner.set(uid,i)));
  const active=document.activeElement, focused=active?.closest?.("#portfolio-blocks") && active.dataset.field ? [active.dataset.block,active.dataset.strategy,active.dataset.field] : null;
  const html=draft.blocks.map((b,i)=>`<fieldset class="portfolio-block"><legend>Блок ${i+1}${b.strategies.length>1 ? " · комбинация" : ""}</legend><div class="block-head"><label>Доля капитала, %<input type="number" min="0.01" max="100" step="0.01" data-block="${i}" data-field="share" value="${esc(b.share)}"></label>${draft.blocks.length>1 ? `<button type="button" class="link-button" data-action="remove-block" data-block="${i}">Удалить блок</button>` : ""}</div>${b.strategies.map((s,j)=>`<div class="block-strategy"><label>Стратегия<select data-block="${i}" data-strategy="${j}" data-field="kind">${Object.keys(KINDS).map(k=>`<option value="${esc(k)}" ${k===s.kind ? "selected" : ""}>${esc(kindName(k))} · ${esc(kindNeed(k))}</option>`).join("")}</select></label><label>Вес<input type="number" min="0.01" step="0.01" data-block="${i}" data-strategy="${j}" data-field="weight" value="${esc(s.weight)}"></label>${b.strategies.length>1 ? `<button type="button" class="link-button" data-action="remove-strategy" data-block="${i}" data-strategy="${j}">Убрать</button>` : ""}</div>${s.kind==="ai-model" ? aiParamsHtml(s,i,j) : ""}`).join("")}${b.strategies.length<6 ? `<button type="button" class="link-button" data-action="add-strategy" data-block="${i}">+ Стратегия в комбинацию</button>` : ""}<div class="block-instruments" aria-label="Инструменты блока ${i+1}">${assets.map(a=>{const other=owner.has(a.uid) && owner.get(a.uid)!==i;return `<button type="button" class="asset-chip" data-action="toggle-instrument" data-block="${i}" data-uid="${esc(a.uid)}" aria-pressed="${b.instruments.includes(a.uid)}" ${other ? `disabled title="Уже в блоке ${owner.get(a.uid)+1}"` : ""}>${esc(a.ticker || a.uid)}${a.kind && a.kind!=="share" ? ` · ${esc(INSTRUMENT_KINDS[a.kind] || a.kind)}` : ""}${other ? ` · блок ${owner.get(a.uid)+1}` : ""}</button>`;}).join("")}</div><p class="detail-note">Выбрано инструментов: ${b.instruments.length} · нужно: ${b.strategies.map(s=>`${kindName(s.kind)} — ${kindNeed(s.kind)}`).join("; ")}</p></fieldset>`).join("");
  // Rebuild only on change, so typing in share/weight fields keeps focus and caret.
  if($("portfolio-blocks").dataset.html!==html){
    $("portfolio-blocks").innerHTML=html;$("portfolio-blocks").dataset.html=html;
    if(focused){const element=$("portfolio-blocks").querySelector(`[data-block="${focused[0]}"]${focused[1]!==undefined ? `[data-strategy="${focused[1]}"]` : ":not([data-strategy])"}[data-field="${focused[2]}"]`);if(element)element.focus();}
  }
  renderPortfolioCheck();
}
// Settings of an ai-model block; the connection (provider, url, token) is a profile in the models file.
function aiParamsHtml(s,i,j) {
  const p=s.params || {}, models=controlsData?.ai_models?.models || [], attr=field=>`data-block="${i}" data-strategy="${j}" data-field="ai:${field}"`;
  const profile=p.model_profile || controlsData?.ai_models?.default || "";
  const options=models.map(m=>`<option value="${esc(m.name)}" ${m.name===profile ? "selected" : ""}>${esc(m.name)} · ${esc(m.provider)} · ${esc(m.model)}</option>`).join("");
  return `<div class="ai-params"><label>Модель (профиль)<select ${attr("model_profile")}>${options || '<option value="">Нет профилей — заполните файл моделей</option>'}</select></label><label>Идея стратегии<textarea rows="3" maxlength="4000" ${attr("instructions")} placeholder="Например: держи бумаги с позитивными новостями и восходящим трендом">${esc(p.instructions || "")}</textarea></label><div class="form-triple"><label>Доля одной бумаги, %<input type="number" min="1" max="100" step="1" value="${esc(p.max_weight_percent ?? "25")}" ${attr("max_weight_percent")}></label><label>Новости за, ч<input type="number" min="1" max="168" step="1" value="${esc(p.news_hours ?? 24)}" ${attr("news_hours")}></label><label class="log-check"><input type="checkbox" ${p.allow_short ? "checked" : ""} ${attr("allow_short")}> Разрешить short</label></div><div class="form-triple"><label>Пересмотр портфеля<select ${attr("decision_interval_minutes")}>${[[60,"на открытии и каждый час"],[30,"на открытии и каждые 30 минут"],[120,"на открытии и каждые 2 часа"],[0,"только на открытии"]].map(([v,t])=>`<option value="${v}" ${Number(p.decision_interval_minutes ?? 60)===v ? "selected" : ""}>${t}</option>`).join("")}</select></label><label>Всплеск новостей, шт. за 30 мин<input type="number" min="0" max="50" step="1" value="${esc(p.news_burst_items ?? 3)}" ${attr("news_burst_items")}></label><label>Скачок цены, %<input type="number" min="0" max="50" step="0.5" value="${esc(p.price_jump_percent ?? "2")}" ${attr("price_jump_percent")}></label></div><div class="form-quad"><label>Важное событие эмитента<select ${attr("material_importance")}>${[[4,"важность 4–5"],[5,"только 5"],[3,"от 3"],[0,"не спрашивать"]].map(([v,t])=>`<option value="${v}" ${Number(p.material_importance ?? 4)===v ? "selected" : ""}>${t}</option>`).join("")}</select></label><label>Индекс Мосбиржи, % с открытия<input type="number" min="0" max="50" step="0.5" value="${esc(p.market_move_percent ?? "2")}" ${attr("market_move_percent")}></label><label>Нефть Brent, %<input type="number" min="0" max="50" step="0.5" value="${esc(p.commodity_move_percent ?? "3")}" ${attr("commodity_move_percent")}></label><label>Юань или доллар, %<input type="number" min="0" max="50" step="0.5" value="${esc(p.currency_move_percent ?? "1.5")}" ${attr("currency_move_percent")}></label></div><p class="detail-note">Модель читает документы (новости, раскрытие эмитентов, сообщения биржи и ЦБ) и состояние рынка, запрашивает индикаторы и выбирает доли бумаг этого блока. Внеочередно её спрашивают при важном событии эмитента и резком движении рынка (0 — не спрашивать). Каждое решение с обоснованием — в «Решениях модели» на странице стратегии.</p></div>`;
}
function aiParamValue(element) {
  const field=element.dataset.field.slice(3);
  if(field==="allow_short")return element.checked;
  if(["news_hours","decision_interval_minutes","news_burst_items","material_importance"].includes(field))return Number(element.value);
  return element.value;
}
function renderPortfolioCheck() {
  const errors=portfolioErrors(portfolioDraft);
  $("portfolio-check").className=`detail-note ${errors.length ? "warning" : "good"}`;
  $("portfolio-check").textContent=errors.length ? errors.join(" · ") : `Готово: ${portfolioDraft.blocks.length} ${portfolioDraft.blocks.length===1 ? "блок" : "блока"}, ${portfolioDraft.blocks.reduce((n,b)=>n+b.instruments.length,0)} инструментов.`;
  const blocked=errors.length>0 || !(controlsData?.available && Date.now()-controlsAt<20000);
  $("portfolio-form").querySelector('[type="submit"]').disabled=blocked;$("portfolio-create").disabled=blocked;
}

function renderControls() {
  if(!$("control-overview"))return;
  const healthy=controlsData?.available && Date.now()-controlsAt<20000;
  const limit=controlsData?.max_runtimes || 4, running=(data?.cases || []).filter(c=>c.running).length;
  if(healthy && document.activeElement!==$("runtime-limit") && !$("runtime-limit").dataset.dirty)$("runtime-limit").value=limit;
  $("runtime-limit").max=controlsData?.runtime_cap || 10;
  $("control-connection").textContent=healthy ? `Подключено · работает ${running} из ${limit} процессов · тариф позволяет до ${controlsData.runtime_cap || 10}` : "Управление недоступно: ожидаем подключение к БД и наблюдателю";
  document.querySelectorAll(".strategy-controls button, #limit-form button").forEach(b=>b.disabled=!healthy);
  const c=data && selectedCase();
  if(c){
    const control=controlFor(c), changed=controlCase!==c.strategy_id;
    const account=data.accounts.find(a=>a.id===c.account_id), assets=allAssets(c);
    if(changed){$("execution-mode").value=control?.execution_mode || "auto";controlCase=c.strategy_id;$("budget-amount").value="";}
    const manualOld=$("manual-instrument").value;
    selectAssets($("manual-instrument"),assets.filter(a=>!a.benchmark));
    if(manualOld && [...$("manual-instrument").options].some(o=>o.value===manualOld))$("manual-instrument").value=manualOld;
    selectAssets($("asset-select"),assets);
    const active=control?.enabled!==false;
    $("control-case-state").textContent=`${active ? "Автоторговля включена" : "Автоторговля выключена"} · ${control?.execution_mode==="confirm" ? "планы требуют подтверждения" : "автоматическая отправка"} · ${c.running && c.service_only ? "работает служебный процесс без сигналов" : c.running ? "торговый процесс работает" : "процесс не запущен"}`;
    const run=[...(controlsData?.ranking?.history || [])].filter(r=>r.strategy_id===c.strategy_id).sort((a,b)=>String(b.started_at).localeCompare(a.started_at))[0];
    $("control-balances").innerHTML=`<span>Портфель<strong>${fmt(account?.equity,2)} ₽</strong></span><span>Денежный остаток<strong>${fmt(account?.cash,2)} ₽</strong></span><span>Бюджет<strong>${fmt(control?.execution_budget ?? c.risk?.budget,2)} ₽</strong></span><span>Прибыль прогона<strong>${run ? fmt(run.pnl_rub,2)+" ₽" : "Нет базы"}</strong></span>`;
    document.querySelector('[data-command="resume"]').disabled=!healthy || (active && c.running);
    document.querySelector('[data-command="pause"]').disabled=!healthy || !active;
    renderSchedule(c,control);
    renderBudget(c,control);
    renderPortfolio(c);
    renderPlans(c);
    if(!healthy)$("approval-plans").querySelectorAll("button").forEach(b=>b.disabled=true);
  }
  if(typeof renderLive==="function")renderLive(c);
  if(typeof renderAi==="function")renderAi(c);
  if(typeof renderCollector==="function")renderCollector();
  if(typeof renderExposures==="function")renderExposures();
  if(typeof renderPortfolios==="function")renderPortfolios();
  if(typeof renderAiBoard==="function")renderAiBoard();
  if(typeof renderCosts==="function")renderCosts();
  renderRanking();
  $("catalog-list").innerHTML=allAssets().map(a=>`<button type="button" class="asset-chip" data-chart-asset="${esc(a.uid)}" title="${esc([a.name,a.kind && (INSTRUMENT_KINDS[a.kind] || a.kind),a.kind ? instrumentDetails(a) : ""].filter(Boolean).join(" · "))}">${esc(a.ticker || a.uid)}${a.kind && a.kind!=="share" ? ` · ${esc(INSTRUMENT_KINDS[a.kind] || a.kind)}` : ""}${c?.assets?.some(x=>x.uid===a.uid&&!x.benchmark) ? " · в стратегии" : ""}${(data?.accounts || []).some(x=>x.id===c?.account_id&&x.positions.some(p=>p.instrument_id===a.uid || p.instrument===a.ticker)) ? " · позиция" : ""}${data?.orders?.some(o=>o.strategy_id===c?.strategy_id&&o.instrument_id===a.uid&&pendingStates.has(o.status)) ? " · заявка" : ""}${a.uid===$("manual-instrument").value ? " · ручная торговля" : ""}${a.benchmark ? " · benchmark" : ""}</button>`).join("");
  const commands=controlsData?.commands || [];
  const finished=new Set(commands.filter(c=>["succeeded","failed"].includes(c.status)).map(c=>c.id));
  for(const key of Object.keys(sessionStorage)){if(key.startsWith("sandbox-command:")&&finished.has(sessionStorage.getItem(key)))sessionStorage.removeItem(key);}
  $("command-history").innerHTML=commands.length ? table(["Время","Действие","Статус","Результат"],commands.map(c=>`<tr><td>${esc(datetime(c.created_at))}</td><td>${esc(commandNames[c.kind] || c.kind)}<span class="subtext mono">${esc(c.id.slice(0,8))}</span></td><td>${badge(commandStates[c.status] || c.status,c.status==="failed" ? "error" : c.status==="uncertain" ? "warn" : "")}</td><td>${esc(commandResult(c))}</td></tr>`)) : empty("Команд пока нет","Действия и их подтверждённые результаты сохраняются здесь.");
}
function renderPlans(c) {
  const states=new Map((controlsData?.batches || []).map(b=>[b.batch_id,b]));
  const targets=(data.targets || []).filter(t=>t.strategy===c.kind && !t.maintenance && !["completed","failed"].includes(t.status));
  $("approval-plans").innerHTML=targets.length ? `<h3>Управление планами</h3>`+targets.map(t=>{
    const gate=states.get(t.batch_id), expired=Date.parse(t.valid_until)<=Date.now();
    const legs=data.orders.filter(o=>o.batch_id===t.batch_id);
    return `<article class="plan-card"><strong>${gate?.status==="awaiting" ? "Ожидает подтверждения" : gate?.status==="rejected" ? "Отмена / сверка" : "План в работе"}</strong><p class="detail-note">Действует до ${esc(datetime(t.valid_until))}${expired ? " · срок истёк" : ""}</p><p>${legs.map(o=>`${esc(o.instrument)}: ${/buy/i.test(o.side) ? "купить" : "продать"} ${fmt(o.lots)} лот.`).join(" · ") || "План без заявок"}</p><div class="control-actions">${gate?.status==="awaiting" ? `<button class="button primary" data-plan-action="approve_batch" data-batch="${esc(t.batch_id)}" ${expired ? "disabled" : ""}>Подтвердить план</button>` : ""}${gate?.revision!==-1 ? `<button class="button" data-plan-action="cancel_batch" data-batch="${esc(t.batch_id)}">Отклонить / отменить весь план</button>` : "Защитное закрытие"}</div></article>`;
  }).join("") : "";
}
function renderRanking() {
  const ranking=controlsData?.ranking, rows=ranking?.[$("ranking-filter").value] || [];
  $("ranking-content").innerHTML=rows.length ? table(["Место / стратегия","Доходность","Прибыль","Просадка","Исполнения / капитал","Начало"],rows.map((r,i)=>`<tr><td>${r.rank || "—"}. ${esc(r.strategy || r.strategy_id)}<span class="subtext">${r.classification==="clean" ? "Автоматический" : "С управлением"} · ${esc(r.id.slice(0,8))}</span></td><td class="num">${fmt(r.return_percent,2)}%</td><td class="num">${fmt(r.pnl_rub,2)} ₽</td><td class="num">${fmt(r.max_drawdown_percent,2)}%</td><td>${fmt(r.fills)}<span class="subtext">${fmt(r.principal)} ₽</span></td><td>${esc(datetime(r.started_at))}<span class="subtext">${r.ended_at ? "Завершён: "+esc(datetime(r.ended_at)) : "Продолжается"} · ${fmt(Math.max(0,((r.ended_at ? Date.parse(r.ended_at) : Date.now())-Date.parse(r.started_at))/3600000),1)} ч</span></td></tr>`)) : empty("Пока нет результатов для рейтинга","Нужен прогон с подтверждёнными исполнениями и базой капитала.");
  $("ranking-unranked").textContent=`Без места в рейтинге: ${ranking?.unranked?.length || 0}. Неполные данные и отсутствие сделок не считаются нулевой доходностью. Ручное вмешательство не исчезает после повторного включения.`;
}
async function refreshControls() {
  if(controlsBusy)return;controlsBusy=true;
  try{const r=await fetch("/api/control",{cache:"no-store",signal:AbortSignal.timeout(10000)});if(!r.ok)throw Error();controlsData=await r.json();controlsAt=Date.now();renderControls();}
  catch(_){controlsAt=0;renderControls();}finally{controlsBusy=false;}
}
async function issue(kind,payload={},global=false) {
  const c=data && selectedCase();
  if(!global&&!c)return null;
  const body={kind,payload,...(global ? {} : {strategy_id:c.strategy_id,account_id:c.account_id})};
  const key="sandbox-command:"+JSON.stringify(body);
  const id=sessionStorage.getItem(key) || crypto.randomUUID();sessionStorage.setItem(key,id);body.id=id;
  const message=$("command-message");message.textContent=`${commandNames[kind]}: отправляем команду ${id.slice(0,8)}…`;
  try{
    if(!csrf){const s=await fetch("/api/session",{cache:"no-store"});if(!s.ok)throw Error("Не удалось открыть локальную сессию");csrf=(await s.json()).csrf_token;}
    const r=await fetch("/api/commands",{method:"POST",headers:{"Content-Type":"application/json","X-CSRF-Token":csrf},body:JSON.stringify(body),signal:AbortSignal.timeout(15000)});
    const value=await r.json();
    // A rejected request was never stored: the next attempt may get a fresh identifier.
    if(!r.ok){if(r.status===403)csrf=null;if(r.status===400)sessionStorage.removeItem(key);throw Error(value.error || "Команда не принята");}
    message.textContent=`${commandNames[kind]}: ${commandStates[value.command.status] || value.command.status}. № ${id.slice(0,8)}. ${commandResult(value.command)}`;
    if(["succeeded","failed"].includes(value.command.status))sessionStorage.removeItem(key);
    await refreshControls();
    return value.command;
  }catch(error){message.textContent=`${error.message}. Команда № ${id.slice(0,8)}: проверьте журнал. Повтор отправки использует тот же идентификатор.`;return null;}
}
$("limit-form").addEventListener("submit",async e=>{e.preventDefault();if(await issue("runtime_limit",{value:Number($("runtime-limit").value)},true))delete $("runtime-limit").dataset.dirty;});
$("runtime-limit").addEventListener("input",()=>{$("runtime-limit").dataset.dirty="true";});
// A real account adds a typed confirmation to «Включить» and «Закрыть позиции» (live.js).
document.querySelectorAll("[data-command]").forEach(b=>b.addEventListener("click",()=>{const payload=typeof livePayload==="function" ? livePayload(b.dataset.command) : {};if(payload)issue(b.dataset.command,payload);}));
$("execution-form").addEventListener("submit",e=>{e.preventDefault();issue("execution_mode",{mode:$("execution-mode").value});});
$("manual-type").addEventListener("change",()=>{$("manual-price").disabled=$("manual-type").value!=="limit";$("manual-price").required=!$("manual-price").disabled;});
$("manual-instrument").addEventListener("change",renderControls);
$("manual-form").addEventListener("submit",e=>{e.preventDefault();const payload={instrument_id:$("manual-instrument").value,side:$("manual-side").value,lots:Number($("manual-lots").value),order_type:$("manual-type").value};if(payload.order_type==="limit")payload.price=$("manual-price").value;issue("manual_order",payload);});
$("budget-amount").addEventListener("input",()=>{const c=data && selectedCase();if(c)renderBudget(c,controlFor(c));});
$("budget-form").addEventListener("submit",e=>{e.preventDefault();issue("budget",{amount:$("budget-amount").value});});
$("payin-form").addEventListener("submit",e=>{e.preventDefault();issue("pay_in",{amount:$("payin-amount").value});});
document.querySelectorAll('[name="run-mode"]').forEach(r=>r.addEventListener("change",()=>{scheduleDraft.run_mode=r.value;scheduleDraft.dirty=true;renderControls();}));
$("schedule-close").addEventListener("change",()=>{scheduleDraft.close_after=$("schedule-close").checked;scheduleDraft.dirty=true;renderControls();});
$("schedule-days").addEventListener("click",e=>{const b=e.target.closest("[data-day]");if(!b || b.disabled)return;const days=scheduleDraft.days;days.has(b.dataset.day) ? days.delete(b.dataset.day) : days.add(b.dataset.day);scheduleDraft.dirty=true;renderControls();});
$("schedule-form").addEventListener("submit",async e=>{
  e.preventDefault();
  const now=Date.now(), open=new Set(tradingSessions().filter(d=>Date.parse(d.endTime)>now).map(d=>d.date));
  const command=await issue("schedule",{run_mode:scheduleDraft.run_mode,days:[...scheduleDraft.days].filter(d=>open.has(d)).sort(),close_after:scheduleDraft.close_after});
  if(command?.status==="succeeded")scheduleDraft.dirty=false;
});
$("portfolio-title").addEventListener("input",()=>{portfolioDraft.title=$("portfolio-title").value;portfolioDraft.dirty=true;renderPortfolioCheck();});
$("portfolio-add-block").addEventListener("click",()=>{portfolioDraft.blocks.push({share:"0",instruments:[],strategies:[{kind:"moving-average-trend",weight:"1"}]});portfolioDraft.dirty=true;renderControls();});
$("portfolio-blocks").addEventListener("click",e=>{
  const b=e.target.closest("[data-action]");if(!b || b.disabled)return;
  const block=portfolioDraft.blocks[Number(b.dataset.block)];
  if(b.dataset.action==="remove-block")portfolioDraft.blocks.splice(Number(b.dataset.block),1);
  if(b.dataset.action==="add-strategy"){const used=new Set(block.strategies.map(s=>s.kind));block.strategies.push({kind:Object.keys(KINDS).find(k=>!used.has(k)),weight:"1"});}
  if(b.dataset.action==="remove-strategy")block.strategies.splice(Number(b.dataset.strategy),1);
  if(b.dataset.action==="toggle-instrument"){const i=block.instruments.indexOf(b.dataset.uid);i<0 ? block.instruments.push(b.dataset.uid) : block.instruments.splice(i,1);}
  portfolioDraft.dirty=true;renderControls();
});
$("portfolio-blocks").addEventListener("input",e=>{
  const t=e.target, block=portfolioDraft.blocks[Number(t.dataset.block)];if(!block || !t.dataset.field)return;
  if(t.dataset.field.startsWith("ai:")){const strategy=block.strategies[Number(t.dataset.strategy)];strategy.params={...(strategy.params || {}),[t.dataset.field.slice(3)]:aiParamValue(t)};portfolioDraft.dirty=true;renderPortfolioCheck();return;}
  if(t.dataset.field==="share")block.share=t.value;
  if(t.dataset.field==="weight")block.strategies[Number(t.dataset.strategy)].weight=t.value;
  if(t.dataset.field==="kind"){const strategy=block.strategies[Number(t.dataset.strategy)];strategy.kind=t.value;if(t.value!=="ai-model")delete strategy.params;portfolioDraft.dirty=true;renderControls();return;}
  portfolioDraft.dirty=true;renderPortfolioCheck();
});
$("portfolio-blocks").addEventListener("change",e=>{if(e.target.dataset.field?.startsWith("ai:") && e.target.type==="checkbox")e.target.dispatchEvent(new Event("input",{bubbles:true}));});
// Strategies of the payload; ai-model blocks also carry their settings.
const blockStrategies=b=>b.strategies.map(s=>({kind:s.kind,weight:String(s.weight),...(s.kind==="ai-model" ? {params:aiPayload(s.params)} : {})}));
function aiPayload(params={}) {
  const out={};
  if(params.model_profile)out.model_profile=params.model_profile;
  if(params.instructions?.trim())out.instructions=params.instructions.trim();
  if(params.max_weight_percent!==undefined && params.max_weight_percent!=="")out.max_weight_percent=String(params.max_weight_percent);
  if(Number.isInteger(params.news_hours))out.news_hours=params.news_hours;
  if(params.allow_short!==undefined)out.allow_short=Boolean(params.allow_short);
  if(Number.isInteger(params.decision_interval_minutes))out.decision_interval_minutes=params.decision_interval_minutes;
  if(Number.isInteger(params.news_burst_items))out.news_burst_items=params.news_burst_items;
  if(params.price_jump_percent!==undefined && params.price_jump_percent!=="")out.price_jump_percent=String(params.price_jump_percent);
  if(Number.isInteger(params.material_importance))out.material_importance=params.material_importance;
  for(const key of ["market_move_percent","commodity_move_percent","currency_move_percent"])
    if(params[key]!==undefined && params[key]!=="")out[key]=String(params[key]);
  return out;
}
$("portfolio-form").addEventListener("submit",async e=>{
  e.preventDefault();
  if(portfolioErrors(portfolioDraft).length)return renderPortfolioCheck();
  const payload={title:portfolioDraft.title.trim(),blocks:portfolioDraft.blocks.map(b=>({instruments:[...b.instruments],share:String(b.share),strategies:blockStrategies(b)}))};
  const command=await issue("portfolio",payload);
  if(command)portfolioDraft.dirty=false;
});
$("portfolio-create").addEventListener("click",async()=>{
  const c=data && selectedCase();if(!c || portfolioErrors(portfolioDraft).length)return renderPortfolioCheck();
  const payload={account_id:c.account_id,title:portfolioDraft.title.trim(),blocks:portfolioDraft.blocks.map(b=>({instruments:[...b.instruments],share:String(b.share),strategies:blockStrategies(b)}))};
  if(await issue("portfolio_create",payload,true))portfolioDraft.dirty=false;
});
$("approval-plans").addEventListener("click",e=>{const b=e.target.closest("[data-plan-action]");if(b)issue(b.dataset.planAction,{batch_id:b.dataset.batch});});
$("ranking-filter").addEventListener("change",renderRanking);
// What matters about an instrument before adding it, by its type.
function instrumentDetails(a) {
  const parts=[`лот ${fmt(a.lot)}`];
  if(a.kind==="futures"){
    if(a.point_value)parts.push(`пункт цены ${fmt(a.point_value,4)} ₽`);
    if(a.margin_buy)parts.push(`ГО ${fmt(a.margin_buy,0)} ₽`);
    if(a.continuous)parts.push(`сейчас ${a.contract}, переход ${date(a.roll_at)}`);
    else if(a.last_trade_date)parts.push(`последний день торгов ${date(a.last_trade_date,{year:"numeric"})}`);
  }
  if(a.kind==="bond"){
    if(a.nominal)parts.push(`номинал ${fmt(a.nominal,2)} ₽`);
    if(a.maturity_date)parts.push(`погашение ${date(a.maturity_date,{year:"numeric"})}`);
    if(a.aci)parts.push(`НКД ${fmt(a.aci,2)} ₽`);
  }
  if(a.qualified_only)parts.push("для квалифицированных");
  return parts.join(" · ");
}
$("search-form").addEventListener("submit",async e=>{e.preventDefault();$("instrument-results").textContent="Поиск…";try{const r=await fetch("/api/instruments/search?q="+encodeURIComponent($("instrument-query").value)+"&kind="+encodeURIComponent($("instrument-kind").value));const result=await r.json();if(!r.ok)throw Error(result.error || "Поиск недоступен");const known=new Set((controlsData?.catalog || []).map(a=>a.uid));$("instrument-results").innerHTML=result.instruments.length ? table(["Тип","Тикер","Название","Подробности",""],result.instruments.map(a=>`<tr class="${a.tradable ? "" : "muted-row"}"><td>${esc(INSTRUMENT_KINDS[a.kind] || a.kind)}${a.continuous ? '<span class="subtext">непрерывный</span>' : ""}</td><td><strong>${esc(a.ticker)}</strong></td><td>${esc(a.name)}</td><td class="subtext-cell">${esc(instrumentDetails(a))}</td><td>${known.has(a.uid) ? '<span class="good">В каталоге</span>' : a.tradable ? `<button class="button" data-add-asset="${esc(a.uid)}">Добавить</button>` : `<span class="muted-text" title="${esc(a.reason)}">Не торгуется: ${esc(a.reason)}</span>`}</td></tr>`)) : "Ничего не найдено";}catch(error){$("instrument-results").textContent=error.message;}});
$("instrument-results").addEventListener("click",e=>{const b=e.target.closest("[data-add-asset]");if(b)issue("catalog_add",{uid:b.dataset.addAsset},true);});
$("catalog-list").addEventListener("click",e=>{const b=e.target.closest("[data-chart-asset]");if(!b)return;if(!selectedCase()){ $("command-message").textContent="Выберите стратегию для просмотра графика инструмента";return;}$("asset-select").value=b.dataset.chartAsset;renderDetailTrades();loadMarket(true);$("asset-chart").scrollIntoView({behavior:"smooth",block:"center"});});
setInterval(()=>{if(!document.hidden){refreshControls();if(data&&selectedCase())loadMarket();}},5000);
refreshControls();
