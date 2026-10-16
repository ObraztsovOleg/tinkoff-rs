"use strict";

let controlsData=null, controlsAt=0, controlsBusy=false, controlCase=null, csrf=null;
const commandNames={pause:"Выключить",resume:"Включить",manual_order:"Ручная заявка",cleanup:"Закрыть позиции",budget:"Бюджет",pay_in:"Пополнение",mode:"Режим стенда",execution_mode:"Режим заявок",cancel_batch:"Отменить план",approve_batch:"Подтвердить план",universe:"Состав инструментов",new_run:"Новый чистый прогон",catalog_add:"Добавить инструмент"};
const commandStates={queued:"В очереди",running:"Выполняется",succeeded:"Выполнено",failed:"Отклонено",uncertain:"Исход требует сверки"};

$("overview").insertAdjacentHTML("afterend", `<section class="panel control-panel" id="control-overview"><div class="section-heading"><div><h2>Управление стендом</h2><p id="control-connection" class="muted">Подключаем управление…</p></div><form id="mode-form" class="inline-form"><label for="operating-mode">Режим</label><select id="operating-mode"><option value="campaign">Кампания по расписанию</option><option value="free">Свободный режим</option></select><button class="button" type="submit">Применить</button></form></div><p id="command-message" class="control-message" role="status" aria-live="polite"></p></section>`);
$("strategy-detail").querySelector(".asset-toolbar").insertAdjacentHTML("beforebegin", `<div class="strategy-controls"><div class="control-actions"><button class="button primary" data-command="resume">Включить</button><button class="button" data-command="pause">Выключить</button><button class="button danger" data-command="cleanup">Закрыть позиции</button><button class="button" data-command="new_run">Новый чистый прогон</button><form id="execution-form" class="inline-form"><label for="execution-mode">Планы</label><select id="execution-mode"><option value="auto">Авто</option><option value="confirm">Подтверждать</option></select><button class="button" type="submit">Сохранить</button></form></div><div id="control-case-state" class="detail-note"></div><div id="control-balances" class="control-balances"></div><div class="control-grid"><form id="manual-form" class="control-form"><h3>Своя заявка</h3><label>Инструмент торговли<select id="manual-instrument" required></select></label><div class="form-pair"><label>Направление<select id="manual-side"><option value="buy">Покупка</option><option value="sell">Продажа</option></select></label><label>Лоты<input id="manual-lots" type="number" min="1" step="1" value="1" required></label></div><div class="form-pair"><label>Тип<select id="manual-type"><option value="market">Рыночная</option><option value="limit">Лимитная</option></select></label><label>Цена, ₽<input id="manual-price" type="number" min="0.000000001" step="any" disabled></label></div><p class="detail-note">Автоторговля будет выключена до вашего включения. Лоты, бюджет и риск проверяются перед отправкой.</p><button type="submit" class="button primary">Отправить ручную заявку</button></form><div><form id="budget-form" class="control-form"><h3>Торговый бюджет</h3><div class="form-pair"><label>Действие<select id="budget-operation"><option value="set">Установить</option><option value="add">Добавить</option><option value="subtract">Вычесть</option></select></label><label>Сумма, ₽<input id="budget-amount" type="number" min="0.01" step="0.01" required></label></div><button class="button" type="submit">Изменить бюджет</button></form><form id="payin-form" class="control-form"><h3>Пополнение sandbox-счёта</h3><label>Сумма, ₽<input id="payin-amount" type="number" min="0.01" max="30000000" step="0.01" required></label><button class="button" type="submit">Пополнить</button><p class="detail-note">Пополнение не меняет бюджет и не считается прибылью. API не позволяет вычитать деньги со счёта.</p></form></div></div><div id="approval-plans"></div><details class="universe-editor"><summary>Изменить набор инструментов стратегии</summary><form id="universe-form"><label>Набор стратегии<select id="universe-select" multiple size="6"></select></label><p class="detail-note">Нужны выключенная стратегия, закрытые позиции и завершённые заявки. Будет создана новая ревизия с повторным прогревом. Выбор графика не меняет этот набор.</p><button class="button" type="submit">Создать ревизию и прогреть</button></form></details></div>`);
$("activity").insertAdjacentHTML("beforebegin", `<section class="panel" id="ranking"><div class="section-heading"><div><h2>Рейтинг стратегий</h2><p class="muted">Доходность относительно выделенного капитала · после комиссий · без пополнений</p></div><label>Выборка<select id="ranking-filter"><option value="clean">Автоматические прогоны</option><option value="mixed">С ручным управлением</option><option value="history">История прогонов</option><option value="unranked">Без места в рейтинге</option></select></label></div><div id="ranking-content"></div><div id="ranking-unranked" class="panel-footnote"></div></section><section class="panel" id="instrument-catalog"><div class="section-heading"><div><h2>Инструменты</h2><p class="muted">Акции MOEX / RUB · добавление в список не меняет состав стратегии</p></div></div><form id="search-form" class="inline-form catalog-search"><label for="instrument-query">Тикер или название</label><input id="instrument-query" placeholder="Например, SBER" maxlength="80" required><button class="button" type="submit">Найти</button></form><div id="instrument-results"></div><div id="catalog-list" class="catalog-list"></div></section><section class="panel"><div class="section-heading"><h2>Журнал управления</h2></div><div id="command-history"></div></section>`);

function commandResult(c) {
  const r=c.result || {};
  if(r.message || r.error)return r.message || r.error;
  if(r.requires_confirmation)return "План изменился: требуется новое подтверждение";
  if(r.execution_budget!==undefined)return `Бюджет: ${fmt(r.execution_budget,2)} ₽ · база риска сохранена`;
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
function renderControls() {
  if(!$("control-overview"))return;
  const healthy=controlsData?.available && Date.now()-controlsAt<20000;
  if(healthy && !$("operating-mode").dataset.loaded){$("operating-mode").value=controlsData.mode;$("operating-mode").dataset.loaded="true";}
  $("control-connection").textContent=healthy ? `Подключено · режим: ${controlsData.mode==="free" ? "свободный" : "кампания"} · максимум 4 процесса` : "Управление недоступно: ожидаем подключение к БД и наблюдателю";
  document.querySelectorAll(".strategy-controls button, #mode-form button").forEach(b=>b.disabled=!healthy);
  const c=data && selectedCase();
  if(c){
    const control=controlFor(c), changed=controlCase!==c.strategy_id;
    const account=data.accounts.find(a=>a.id===c.account_id), assets=allAssets(c);
    if(changed){$("execution-mode").value=control?.execution_mode || "auto";controlCase=c.strategy_id;}
    const manualOld=$("manual-instrument").value;
    selectAssets($("manual-instrument"),assets.filter(a=>!a.benchmark));
    selectAssets($("universe-select"),assets.filter(a=>!a.benchmark));
    if(changed){for(const option of $("universe-select").options)option.selected=(c.assets || []).some(a=>a.uid===option.value&&!a.benchmark);}
    if(manualOld && [...$("manual-instrument").options].some(o=>o.value===manualOld))$("manual-instrument").value=manualOld;
    selectAssets($("asset-select"),assets);
    const active=control?.enabled!==false;
    $("control-case-state").textContent=`${active ? "Автоторговля включена" : "Автоторговля выключена"} · ${control?.execution_mode==="confirm" ? "планы требуют подтверждения" : "автоматическая отправка"} · ${c.running ? "процесс работает" : "процесс ожидает запуска"}. ${c.state || ""}`;
    const run=[...(controlsData?.ranking?.history || [])].filter(r=>r.strategy_id===c.strategy_id).sort((a,b)=>String(b.started_at).localeCompare(a.started_at))[0];
    $("control-balances").innerHTML=`<span>Портфель<strong>${fmt(account?.equity,2)} ₽</strong></span><span>Денежный остаток<strong>${fmt(account?.cash,2)} ₽</strong></span><span>Бюджет<strong>${fmt(control?.execution_budget ?? c.risk?.budget,2)} ₽</strong></span><span>Прибыль прогона<strong>${run ? fmt(run.pnl_rub,2)+" ₽" : "Нет базы"}</strong></span>`;
    document.querySelector('[data-command="resume"]').disabled=!healthy || (active && c.running);
    document.querySelector('[data-command="pause"]').disabled=!healthy || !active;
    $("universe-form").querySelector("button").disabled=!healthy || active;
    renderPlans(c);
    if(!healthy)$("approval-plans").querySelectorAll("button").forEach(b=>b.disabled=true);
  }
  renderRanking();
  $("catalog-list").innerHTML=allAssets().map(a=>`<button type="button" class="asset-chip" data-chart-asset="${esc(a.uid)}">${esc(a.ticker || a.uid)}${c?.assets?.some(x=>x.uid===a.uid&&!x.benchmark) ? " · в стратегии" : ""}${(data?.accounts || []).some(x=>x.id===c?.account_id&&x.positions.some(p=>p.instrument_id===a.uid || p.instrument===a.ticker)) ? " · позиция" : ""}${data?.orders?.some(o=>o.strategy_id===c?.strategy_id&&o.instrument_id===a.uid&&pendingStates.has(o.status)) ? " · заявка" : ""}${a.uid===$("manual-instrument").value ? " · ручная торговля" : ""}${a.benchmark ? " · benchmark" : ""}</button>`).join("");
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
  if(!global&&!c)return;
  const body={kind,payload,...(global ? {} : {strategy_id:c.strategy_id,account_id:c.account_id})};
  const key="sandbox-command:"+JSON.stringify(body);
  const id=sessionStorage.getItem(key) || crypto.randomUUID();sessionStorage.setItem(key,id);body.id=id;
  const message=$("command-message");message.textContent=`${commandNames[kind]}: отправляем команду ${id.slice(0,8)}…`;
  try{
    if(!csrf){const s=await fetch("/api/session",{cache:"no-store"});if(!s.ok)throw Error("Не удалось открыть локальную сессию");csrf=(await s.json()).csrf_token;}
    const r=await fetch("/api/commands",{method:"POST",headers:{"Content-Type":"application/json","X-CSRF-Token":csrf},body:JSON.stringify(body),signal:AbortSignal.timeout(15000)});
    const value=await r.json();if(!r.ok){if(r.status===403)csrf=null;throw Error(value.error || "Команда не принята");}
    message.textContent=`${commandNames[kind]}: ${commandStates[value.command.status] || value.command.status}. № ${id.slice(0,8)}. Результат — в журнале управления.`;
    if(["succeeded","failed"].includes(value.command.status))sessionStorage.removeItem(key);
    await refreshControls();
  }catch(error){message.textContent=`${error.message}. Команда № ${id.slice(0,8)}: проверьте журнал. Повтор отправки использует тот же идентификатор.`;}
}
$("mode-form").addEventListener("submit",e=>{e.preventDefault();issue("mode",{mode:$("operating-mode").value},true);});
document.querySelectorAll("[data-command]").forEach(b=>b.addEventListener("click",()=>issue(b.dataset.command)));
$("execution-form").addEventListener("submit",e=>{e.preventDefault();issue("execution_mode",{mode:$("execution-mode").value});});
$("manual-type").addEventListener("change",()=>{$("manual-price").disabled=$("manual-type").value!=="limit";$("manual-price").required=!$("manual-price").disabled;});
$("manual-instrument").addEventListener("change",renderControls);
$("manual-form").addEventListener("submit",e=>{e.preventDefault();const payload={instrument_id:$("manual-instrument").value,side:$("manual-side").value,lots:Number($("manual-lots").value),order_type:$("manual-type").value};if(payload.order_type==="limit")payload.price=$("manual-price").value;issue("manual_order",payload);});
$("budget-form").addEventListener("submit",e=>{e.preventDefault();issue("budget",{operation:$("budget-operation").value,amount:$("budget-amount").value});});
$("payin-form").addEventListener("submit",e=>{e.preventDefault();issue("pay_in",{amount:$("payin-amount").value});});
$("universe-form").addEventListener("submit",e=>{e.preventDefault();issue("universe",{instruments:[...$("universe-select").selectedOptions].map(o=>o.value)});});
$("approval-plans").addEventListener("click",e=>{const b=e.target.closest("[data-plan-action]");if(b)issue(b.dataset.planAction,{batch_id:b.dataset.batch});});
$("ranking-filter").addEventListener("change",renderRanking);
$("search-form").addEventListener("submit",async e=>{e.preventDefault();$("instrument-results").textContent="Поиск…";try{const r=await fetch("/api/instruments/search?q="+encodeURIComponent($("instrument-query").value));const result=await r.json();if(!r.ok)throw Error(result.error || "Поиск недоступен");$("instrument-results").innerHTML=result.instruments.length ? table(["Тикер","Название","Лот",""],result.instruments.map(a=>`<tr><td>${esc(a.ticker)}</td><td>${esc(a.name)}</td><td>${fmt(a.lot)}</td><td><button class="button" data-add-asset="${esc(a.uid)}">Добавить</button></td></tr>`)) : "Доступных акций MOEX/RUB не найдено";}catch(error){$("instrument-results").textContent=error.message;}});
$("instrument-results").addEventListener("click",e=>{const b=e.target.closest("[data-add-asset]");if(b)issue("catalog_add",{uid:b.dataset.addAsset},true);});
$("catalog-list").addEventListener("click",e=>{const b=e.target.closest("[data-chart-asset]");if(!b)return;if(!selectedCase()){ $("command-message").textContent="Выберите стратегию для просмотра графика инструмента";return;}$("asset-select").value=b.dataset.chartAsset;renderDetailTrades();loadMarket(true);$("asset-chart").scrollIntoView({behavior:"smooth",block:"center"});});
setInterval(()=>{if(!document.hidden){refreshControls();if(data&&selectedCase())loadMarket();}},5000);
refreshControls();
