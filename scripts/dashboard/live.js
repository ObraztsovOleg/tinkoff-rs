"use strict";

// Real accounts: connection state, emergency stop, the sandbox gate, the read-only
// account survey and promotion of a sandbox-tested portfolio. Trading needs the
// TINKOFF_TOKEN_PRODUCTION token and no emergency stop.
let gateDirty=false, promoteDirty=false, gateAll=false;
const GATE_ROWS=8;
Object.assign(commandNames,{live_accounts:"Проверка реальных счетов",live_promote:"Перевод на реальный счёт",live_halt:"Аварийная остановка",live_resume:"Снятие аварийной остановки",live_gate:"Порог допуска"});
const ACCOUNT_TYPES={ACCOUNT_TYPE_TINKOFF:"Брокерский",ACCOUNT_TYPE_TINKOFF_IIS:"ИИС",ACCOUNT_TYPE_INVEST_BOX:"Инвесткопилка"};
const ACCESS={ACCOUNT_ACCESS_LEVEL_FULL_ACCESS:"полный доступ",ACCOUNT_ACCESS_LEVEL_READ_ONLY:"только чтение",ACCOUNT_ACCESS_LEVEL_NO_ACCESS:"нет доступа"};
const confirmCode=account=>String(account || "").slice(-4);

$("control-overview").insertAdjacentHTML("afterend", `<section class="panel live-panel" id="live"><div class="section-heading"><div><div class="eyebrow live-eyebrow">РЕАЛЬНЫЕ ДЕНЬГИ</div><h2>Реальные счета</h2><p class="muted">Портфель попадает сюда только после проверки в песочнице — так же и портфель с ИИ-моделью: переносятся модель, её настройки, память и тезисы. Каждый план подтверждается вручную.</p></div><button id="live-halt" class="button danger" type="button">Аварийная остановка</button></div><div id="live-status" class="live-status" role="status"></div><div class="live-grid"><div><h3>Готовность портфелей песочницы</h3><div id="live-gate"></div></div><form id="live-gate-form" class="control-form"><h3>Порог допуска</h3><label>Торговых сессий в Sandbox, не меньше<input id="gate-sessions" type="number" min="1" max="60" step="1" required></label><label>Исполненных сделок, не меньше<input id="gate-fills" type="number" min="0" max="1000" step="1" required></label><label>Решений модели для ИИ-портфелей, не меньше<input id="gate-ai" type="number" min="0" max="1000" step="1" required></label><label class="log-check"><input id="gate-risk" type="checkbox"> Без риск-блокировок в Sandbox</label><button class="button" type="submit">Сохранить порог</button></form></div><div class="live-accounts"><div class="log-heading"><div><h3>Счета токена <span id="live-account-count" class="count-pill">0</span></h3><p id="live-accounts-meta" class="detail-note"></p></div><button id="live-refresh" class="button" type="button">↻ Проверить счета</button></div><div id="live-accounts"></div></div><div id="live-cases"></div><form id="live-promote-form" class="control-form live-promote"><h3>Перевести портфель на реальный счёт</h3><div class="form-pair"><label>Портфель песочницы<select id="promote-source" required></select></label><label>Пустой реальный счёт<select id="promote-account" required></select></label></div><p id="promote-gate" class="detail-note"></p><div class="form-pair"><label>Имя (латиница, цифры, дефис)<input id="promote-kind" maxlength="41" required></label><label>Название<input id="promote-title" maxlength="60" required></label></div><div class="form-triple"><label>Бюджет, ₽<input id="promote-budget" type="number" min="0.01" step="0.01" required></label><label>Дневной стоп, %<input id="promote-daily" type="number" min="0.01" max="100" step="0.01" value="2" required></label><label>Стоп по просадке, %<input id="promote-drawdown" type="number" min="0.01" max="100" step="0.01" value="10" required></label></div><label>Подтверждение: последние 4 цифры номера счёта<input id="promote-confirm" inputmode="numeric" maxlength="4" autocomplete="off" required></label><p class="detail-note">Состав и параметры стратегий (в том числе ИИ-модель, её память и тезисы) копируются из песочницы без изменений. Портфель создаётся выключенным; включение — отдельным действием. Каждый план ждёт вашего подтверждения. Защитное закрытие при срабатывании стопа исполняется без подтверждения. Пополнение и ручные заявки — в приложении брокера.</p><p id="promote-check" class="detail-note" role="status"></p><button class="button live" type="submit">Перевести и прогреть</button></form></section>`);
$("strategy-detail").querySelector(".strategy-controls").insertAdjacentHTML("afterbegin", `<div id="live-banner" class="live-banner" hidden><strong>Реальный счёт · реальные деньги</strong><span id="live-banner-text"></span><label>Последние 4 цифры счёта для «Включить» и «Закрыть позиции»<input id="live-confirm" inputmode="numeric" maxlength="4" autocomplete="off"></label></div>`);

function liveData() {return controlsData?.live;}
function liveHealthy() {return controlsData?.available && Date.now()-controlsAt<20000 && liveData();}
// Payload for account commands: a real account needs the typed confirmation.
function livePayload(kind) {
  const c=data && selectedCase();
  if(!c || c.contour!=="production" || !["resume","cleanup"].includes(kind))return {};
  const code=$("live-confirm").value.trim();
  if(code!==confirmCode(c.account_id)){
    $("command-message").textContent="Реальный счёт: введите последние 4 цифры номера счёта, чтобы подтвердить действие";
    $("live-confirm").focus();
    return null;
  }
  $("live-confirm").value="";
  return {confirm:code};
}
function gateText(row,settings) {
  if(!row)return "";
  const parts=[`сессий ${row.sessions} из ${settings.min_sessions}`,row.ai ? `решений модели ${row.ai_decisions} из ${settings.min_ai_decisions}` : `сделок ${row.fills} из ${settings.min_fills}`,row.risk_blocks ? `риск-блокировок: ${row.risk_blocks}` : "риск-блокировок не было"];
  return parts.join(" · ");
}
function renderLiveStatus(L) {
  const s=L.status, item=(ok,text,hint)=>`<span class="live-check ${ok ? "ok" : "off"}">${ok ? "✓" : "✕"} ${esc(text)}${hint ? `<small>${esc(hint)}</small>` : ""}</span>`;
  $("live-status").innerHTML=item(s.token,s.token ? "Токен TINKOFF_TOKEN_PRODUCTION подключён" : "Токен TINKOFF_TOKEN_PRODUCTION не подключён",s.token ? "" : "compose-setup.py init, затем docker compose up -d")
    +item(!L.halted,L.halted ? `Аварийная остановка с ${datetime(L.halted_at)}` : "Аварийной остановки нет",L.halted ? L.halt_reason || "" : "");
  const halt=$("live-halt");
  halt.textContent=L.halted ? "Снять аварийную остановку" : "Аварийная остановка";
  halt.className=`button ${L.halted ? "" : "danger"}`;
  halt.dataset.kind=L.halted ? "live_resume" : "live_halt";
}
function renderGate(L) {
  const all=L.gate || [], rows=gateAll ? all : all.slice(0,GATE_ROWS);
  $("live-gate").innerHTML=(all.length ? table(["Портфель","Сессии","Сделки / решения ИИ","Риск","Допуск"],rows.map(r=>`<tr><td>${esc(r.title)}${r.ai ? ' <span class="badge">ИИ</span>' : ""}<span class="subtext">${esc(r.kind)}${r.enabled ? " · работает" : " · выключен"}</span></td><td class="num">${fmt(r.sessions)} / ${fmt(L.settings.min_sessions)}</td><td class="num">${r.ai ? `${fmt(r.ai_decisions)} / ${fmt(L.settings.min_ai_decisions)}<span class="subtext">решений модели</span>` : `${fmt(r.fills)} / ${fmt(L.settings.min_fills)}`}</td><td>${r.risk_blocks ? badge(`Блокировок: ${r.risk_blocks}`,"error") : '<span class="good">Без блокировок</span>'}</td><td>${r.passed ? badge("Готов","") : `${badge("Не готов","waiting")}<span class="subtext">${esc(r.failures.join("; "))}</span>`}</td></tr>`))
    : empty("Нет портфелей Sandbox","Соберите и запустите портфель в песочнице — после проверки его можно перевести сюда."))
    +(all.length>GATE_ROWS ? `<button type="button" class="link-button" id="gate-toggle">${gateAll ? "Свернуть" : `Показать все (${all.length})`}</button>` : "");
  if(!gateDirty && !$("live-gate-form").contains(document.activeElement)){
    $("gate-sessions").value=L.settings.min_sessions;$("gate-fills").value=L.settings.min_fills;$("gate-ai").value=L.settings.min_ai_decisions ?? 5;$("gate-risk").checked=L.settings.require_no_risk_block;
  }
}
function renderLiveAccounts(L) {
  const accounts=L.accounts || [];
  $("live-account-count").textContent=accounts.length;
  $("live-accounts-meta").textContent=L.accounts_error ? L.accounts_error : L.accounts_observed_at ? `Проверено ${datetime(L.accounts_observed_at)} МСК · только чтение: заявки при проверке не отправляются` : "Счета ещё не проверялись. Нужен подключённый токен реального счёта.";
  $("live-accounts-meta").className=`detail-note ${L.accounts_error ? "warning" : ""}`;
  $("live-accounts").innerHTML=accounts.length ? table(["Счёт","Тип / доступ","Свободные ₽","Бумаги / заявки","Подключение"],accounts.map(a=>`<tr><td>${esc(a.name || "Без названия")}<span class="subtext mono">${esc(a.id)}</span></td><td>${esc(ACCOUNT_TYPES[a.type] || a.type || "—")}<span class="subtext">${esc(ACCESS[a.access_level] || a.access_level || "—")}</span></td><td class="num">${fmt(a.cash_rub,2)}</td><td class="num">${a.holdings==null ? "—" : `${fmt(a.holdings)} / ${fmt(a.orders)}`}</td><td>${a.eligible ? badge("Пустой, можно подключить","") : `${badge("Не подходит","waiting")}<span class="subtext">${esc((a.problems || []).join("; "))}</span>`}</td></tr>`))
    : empty("Список счетов пуст","Нажмите «Проверить счета». Подключаются только пустые выделенные счета: без бумаг, заявок и заблокированных средств.");
}
function renderLiveCases() {
  const cases=(data?.cases || []).filter(c=>c.contour==="production");
  $("live-cases").innerHTML=cases.length ? `<h3>Реальные портфели</h3>`+table(["Портфель","Счёт","Состояние","Бюджет / стопы","Из Sandbox"],cases.map(c=>{
    const control=controlFor(c), limits=c.risk_limits || {};
    return `<tr class="live-row"><td><button type="button" class="strategy-link" data-live-strategy="${esc(c.kind)}">${esc(displayName(c))}</button></td><td class="mono">${esc(c.account_id)}</td><td>${badge(c.running && !c.service_only ? "Торгует" : status(c.state),c.running ? "" : "waiting")}<span class="subtext">${control?.enabled ? "Включена" : "Выключена"} · планы подтверждаются</span></td><td>${fmt(control?.execution_budget ?? limits.budget,2)} ₽<span class="subtext">день ${esc(limits.daily_loss_percent || "—")}% · просадка ${esc(limits.max_drawdown_percent || "—")}%</span></td><td>${esc(c.source || "—")}<span class="subtext">${esc(datetime(c.promoted_at))}</span></td></tr>`;
  })) : "";
}
function renderPromote(L) {
  const settings=L.settings, sources=L.gate || [], accounts=(L.accounts || []).filter(a=>a.eligible);
  const keepSource=$("promote-source").value, keepAccount=$("promote-account").value;
  const sourceOptions=sources.map(r=>`<option value="${esc(r.strategy_id)}">${esc(r.title)} · ${r.passed ? "готов" : "не готов"}</option>`).join("");
  if($("promote-source").dataset.options!==sourceOptions){$("promote-source").innerHTML=sourceOptions;$("promote-source").dataset.options=sourceOptions;if(sources.some(r=>r.strategy_id===keepSource))$("promote-source").value=keepSource;}
  const accountOptions=accounts.map(a=>`<option value="${esc(a.id)}">${esc(a.name || a.id)} · ${esc(a.id)} · ${fmt(a.cash_rub,2)} ₽</option>`).join("");
  if($("promote-account").dataset.options!==accountOptions){$("promote-account").innerHTML=accountOptions || '<option value="">Нет пустых счетов</option>';$("promote-account").dataset.options=accountOptions;if(accounts.some(a=>a.id===keepAccount))$("promote-account").value=keepAccount;}
  const source=sources.find(r=>r.strategy_id===$("promote-source").value), account=accounts.find(a=>a.id===$("promote-account").value);
  if(source && !promoteDirty){
    $("promote-kind").value=("live-"+source.kind).slice(0,41);$("promote-title").value=(source.title+" · реальный").slice(0,60);
  }
  $("promote-gate").textContent=source ? `${gateText(source,settings)}${source.passed ? " — порог пройден" : " — "+source.failures.join("; ")}` : "Нет портфелей для перевода";
  $("promote-gate").className=`detail-note ${source?.passed ? "good" : "warning"}`;
  const problems=[];
  if(!L.status.token)problems.push("подключите токен TINKOFF_TOKEN_PRODUCTION");
  if(L.halted)problems.push("снимите аварийную остановку");
  if(!source?.passed)problems.push("портфель не прошёл порог допуска");
  if(!account)problems.push("выберите пустой проверенный счёт");
  const budget=Number($("promote-budget").value);
  if(account && $("promote-budget").value && budget>account.cash_rub)problems.push(`бюджет больше свободных рублей (${fmt(account.cash_rub,2)} ₽)`);
  if(Number($("promote-daily").value)>Number($("promote-drawdown").value))problems.push("дневной стоп больше стопа по просадке");
  if(account && $("promote-confirm").value.trim()!==confirmCode(account.id))problems.push("введите последние 4 цифры номера счёта");
  $("promote-check").textContent=problems.length ? "Чтобы перевести: "+problems.join("; ")+"." : `Будет создан выключенный портфель на счёте ${account.id}. Торговлю включите отдельно.`;
  $("promote-check").className=`detail-note ${problems.length ? "warning" : "good"}`;
  $("live-promote-form").querySelector('[type="submit"]').disabled=problems.length>0 || !liveHealthy();
}
// Detail of a real-account strategy: hide sandbox-only tools, lock confirmation mode.
function renderLiveCase(c) {
  const live=c?.contour==="production";
  $("live-banner").hidden=!live;
  $("strategy-detail").classList.toggle("live-detail",live);
  for(const id of ["manual-form","payin-form","portfolio-editor"])$(id).hidden=live;
  document.querySelector('[data-command="new_run"]').hidden=live;
  $("execution-mode").disabled=live;
  if(live){
    $("execution-mode").value="confirm";
    $("execution-form").querySelector("button").disabled=true;
    $("schedule-close").checked=false;$("schedule-close").disabled=true;
    if(scheduleDraft)scheduleDraft.close_after=false;
    const L=liveData(), blocked=!L ? "" : L.halted ? " Включена аварийная остановка." : !L.status.token ? " Токен TINKOFF_TOKEN_PRODUCTION не подключён." : "";
    $("live-banner-text").textContent=`Счёт ${c.account_id}. Каждый план ждёт подтверждения; позиции не закрываются по расписанию.${blocked}`;
  }
}
function renderLive(c) {
  const L=liveData();
  $("live").hidden=!L;
  renderLiveCase(c);
  if(!L)return;
  renderLiveStatus(L);
  renderGate(L);
  renderLiveAccounts(L);
  renderLiveCases();
  renderPromote(L);
  const healthy=liveHealthy();
  for(const b of [$("live-halt"),$("live-refresh"),$("live-gate-form").querySelector("button")])b.disabled=!healthy;
  $("live-refresh").disabled=!healthy || !L.status.token;
}

$("live-halt").addEventListener("click",()=>{
  const kind=$("live-halt").dataset.kind;
  issue(kind,kind==="live_halt" ? {reason:"Аварийная остановка из панели"} : {},true);
});
$("live-refresh").addEventListener("click",()=>issue("live_accounts",{},true));
$("live-gate").addEventListener("click",e=>{if(e.target.id==="gate-toggle"){gateAll=!gateAll;renderGate(liveData());}});
$("live-gate-form").addEventListener("input",()=>{gateDirty=true;});
$("live-gate-form").addEventListener("submit",async e=>{
  e.preventDefault();
  const command=await issue("live_gate",{min_sessions:Number($("gate-sessions").value),min_fills:Number($("gate-fills").value),min_ai_decisions:Number($("gate-ai").value),require_no_risk_block:$("gate-risk").checked},true);
  if(command?.status==="succeeded")gateDirty=false;
});
$("live-promote-form").addEventListener("input",e=>{if(["promote-kind","promote-title"].includes(e.target.id))promoteDirty=true;if(liveData())renderPromote(liveData());});
$("promote-source").addEventListener("change",()=>{promoteDirty=false;if(liveData())renderPromote(liveData());});
$("live-promote-form").addEventListener("submit",async e=>{
  e.preventDefault();
  const payload={source_strategy_id:$("promote-source").value,account_id:$("promote-account").value,kind:$("promote-kind").value.trim(),title:$("promote-title").value.trim(),
    budget:Number($("promote-budget").value).toFixed(2),daily_loss_percent:String(Number($("promote-daily").value)),max_drawdown_percent:String(Number($("promote-drawdown").value)),confirm:$("promote-confirm").value.trim()};
  if(await issue("live_promote",payload,true)){$("promote-confirm").value="";promoteDirty=false;}
});
$("live-cases").addEventListener("click",e=>{const b=e.target.closest("[data-live-strategy]");if(b)chooseStrategy(b.dataset.liveStrategy);});
