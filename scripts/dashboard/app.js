"use strict";

const $ = id => document.getElementById(id);
const esc = value => String(value ?? "").replace(/[&<>"']/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));
const fmt = (value, digits = 0) => value == null || !Number.isFinite(Number(value)) ? "—" : new Intl.NumberFormat("ru-RU", {maximumFractionDigits: digits}).format(Number(value));
const date = (value, options = {}) => {
  if (!value || !Number.isFinite(Date.parse(value))) return "—";
  return new Intl.DateTimeFormat("ru-RU", {timeZone:"Europe/Moscow", day:"numeric", month:"short", ...options}).format(new Date(value));
};
const datetime = value => date(value, {hour:"2-digit", minute:"2-digit"});
const svgX = event => {
  const point = event.currentTarget.createSVGPoint();
  point.x = event.clientX;point.y = event.clientY;
  return point.matrixTransform(event.currentTarget.getScreenCTM().inverse()).x;
};
const age = value => value ? Math.max(0, (Date.now() - Date.parse(value)) / 1000) : Infinity;
const ageText = value => !value ? "нет данных" : age(value) < 60 ? "только что" : `${fmt(Math.floor(age(value) / 60))} мин назад`;
const badge = (text, type = "waiting") => `<span class="badge ${esc(type)}">${esc(text)}</span>`;
const empty = (title, description) => `<div class="empty-state"><span class="empty-icon">⌁</span><strong>${esc(title)}</strong>${esc(description)}</div>`;
const table = (headers, rows) => `<div class="table-wrap"><table><thead><tr>${headers.map(h => `<th>${esc(h)}</th>`).join("")}</tr></thead><tbody>${rows.join("")}</tbody></table></div>`;
const side = value => /buy/i.test(value || "") ? '<span class="side-buy">Покупка</span>' : /sell/i.test(value || "") ? '<span class="side-sell">Продажа</span>' : "—";
const statusNames = {prepared:"Ожидает сессию",running:"Запущена",cleaned:"Завершена",cleanup_pending:"Закрытие позиций","waiting for predecessor cleanup":"Ожидает счёт","failed; coverage incomplete":"Запуск не удался","restart backoff":"Пауза перед перезапуском",filled:"Исполнена",partially_filled:"Частично",pending:"Подготовлена",submitted:"Отправлена",cancelled:"Отменена",canceled:"Отменена",rejected:"Отклонена",failed:"Ошибка",new:"Новая"};
const status = value => {
  const normalized = String(value || "").toLowerCase().replace("execution_report_status_", "");
  return statusNames[normalized] || value || "—";
};
let data = null, activeTab = "orders", busy = false, chartPoints = [];

function options(id, items, first) {
  const select = $(id), old = select.value;
  select.innerHTML = `<option value="">${esc(first)}</option>` + items.map(([value, label]) => `<option value="${esc(value)}">${esc(label)}</option>`).join("");
  if (items.some(([value]) => value === old)) select.value = old;
}

function selectedCase() { return data.cases.find(c => c.kind === $("strategy-filter").value); }
function scopedAccounts() {
  const selected = selectedCase();
  return data.accounts.filter(a => !selected || a.id === selected.account_id);
}

function render() {
  const {campaign, accounts, cases} = data;
  $("campaign-id").textContent = campaign.id || "Кампания не подготовлена";
  const observing = campaign.supervisor_running;
  $("connection").textContent = observing ? "● Наблюдатель работает" : campaign.phase === "complete" ? "Наблюдение завершено" : "Наблюдатель остановлен";
  $("connection").className = `badge ${observing ? "" : "warn"}`;
  const warnings = [...data.warnings];
  if (age(data.updated.broker) > 600) warnings.push("Данные брокера старше 10 минут или отсутствуют. Значения ниже могут быть неактуальны.");
  if (age(data.updated.database) > 180) warnings.push("Отчёт стратегий не обновлялся более трёх минут.");
  $("notice").hidden = warnings.length === 0;
  $("notice").textContent = warnings.join(" ");
  $("sessions").innerHTML = campaign.sessions.map((s, index) => {
    const past = Date.now() > Date.parse(s.endTime), live = Date.now() >= Date.parse(s.startTime) && !past;
    return `<div class="session ${past ? "past" : live ? "live" : ""}"><span class="number">0${index+1}</span><strong>${esc(date(s.date))}</strong><small>${past ? "прошла" : live ? "идёт" : "ожидание"}</small></div>`;
  }).join("");
  const next = campaign.sessions.find(s => Date.parse(s.startTime) > Date.now());
  $("next-session").textContent = next ? `Следующее открытие: ${datetime(next.startTime)} МСК` : "Пять запланированных сессий";
  const known = accounts.filter(a => a.equity != null);
  $("total-equity").textContent = known.length === accounts.length && accounts.length ? fmt(known.reduce((sum,a) => sum+a.equity,0)) : "—";
  $("account-count").textContent = `${accounts.length} уникальных счетов · виртуальные ₽${known.length < accounts.length ? ` · доступно ${known.length}` : ""}`;
  $("strategy-count").textContent = cases.length;
  $("runtime-count").textContent = `${cases.filter(c => c.running).length} работают · ${cases.filter(c => c.preflight_result === "passed" || c.readiness_observed).length} прогреты`;
  const upcoming = cases.filter(c => c.launch_at && !c.running && c.state !== "cleaned" && Date.parse(c.launch_at) > Date.now()).sort((a,b) => Date.parse(a.launch_at)-Date.parse(b.launch_at));
  const waiting = !cases.some(c => c.running) && upcoming.length > 0;
  $("schedule-explanation").hidden = !waiting;
  if (waiting) {
    $("schedule-explanation").textContent = `Стратегии ожидают запланированные полные сессии. Ближайший запуск и прогрев: ${datetime(upcoming[0].launch_at)} МСК, открытие: ${datetime(upcoming[0].opening_at)} МСК.${observing ? " Наблюдатель запустит группу автоматически." : " Наблюдатель сейчас остановлен."} Графики инструментов доступны уже сейчас — нажмите на стратегию.`;
    $("runtime-count").textContent = `Ожидание по расписанию · запуск ${datetime(upcoming[0].launch_at)}`;
  }
  $("open-orders").textContent = accounts.every(a => a.available) && accounts.length ? accounts.reduce((sum,a) => sum+a.active_orders.length,0) : "—";
  const filled = data.orders.filter(o => o.status === "filled");
  $("filled-orders").textContent = data.updated.database ? filled.filter(o => !o.maintenance).length : "—";
  $("filled-note").textContent = `Стратегии · служебных закрытий: ${filled.filter(o => o.maintenance).length}`;
  $("strategies-total").textContent = cases.length;
  options("strategy-filter", cases.map(c => [c.kind,c.kind]), "Все стратегии");
  options("chart-account", accounts.map(a => [a.id,`${a.id.slice(0,8)} · ${a.owner || a.cases[0] || "счёт"}`]), "Все счета");
  renderStrategies(); renderActivity(); renderAccounts(); renderChart();
  renderDetail();
  $("db-updated").textContent = `Отчёт стратегий: ${datetime(data.updated.database)} МСК · ${ageText(data.updated.database)}`;
  $("broker-updated").textContent = `Брокер: ${datetime(data.updated.broker)} МСК · ${ageText(data.updated.broker)}`;
}

function renderStrategies() {
  const filter = $("strategy-filter").value;
  $("strategy-rows").innerHTML = data.cases.filter(c => !filter || c.kind === filter).map(c => {
    const ready = c.preflight_result === "passed" || c.readiness_observed;
    let state = status(c.state), type = "waiting";
    if (c.running) {state = "Работает";type = "";}
    else if (c.state === "running") {state = "Процесс остановлен";type = "warn";}
    if (/failed|pending/.test(c.state)) type = "warn";
    const zero = Math.max(0,(c.natural_target_count || 0)-(c.nonzero_target_count || 0));
    const riskText = c.risk.blocked ? '<span class="badge error">Риск: блокировка</span>' : c.risk.saved ? '<span class="good">Риск сохранён</span>' : '<span class="muted-text">Риск: ожидает</span>';
    return `<tr data-strategy="${esc(c.kind)}"><td><button type="button" class="strategy-link" data-strategy="${esc(c.kind)}">${esc(c.kind)}</button><span class="subtext">${esc(c.account_id.slice(0,8))}${c.predecessor && !c.owns_account ? " · счёт после " + esc(c.predecessor) : ""}</span></td><td>${c.cohort === 0 ? "1–2" : c.cohort+2}</td><td>${badge(state,type)}${!c.running && c.launch_at && c.state !== "cleaned" ? `<span class="subtext">Запуск ${esc(datetime(c.launch_at))} МСК</span>` : ""}${c.failed_batches ? `<span class="subtext warning">Ошибок пакетов: ${fmt(c.failed_batches)}</span>` : ""}</td><td>${ready ? '<span class="good">✓ Подтверждён</span>' : '<span class="muted-text">Ожидает</span>'}</td><td class="num">${fmt(c.natural_decision_count || 0)} / ${fmt(c.natural_target_count || 0)}<span class="subtext">нулевых целей: ${fmt(zero)}</span></td><td class="num">${fmt(c.strategy_filled_intents || 0)}</td><td>${riskText}<span class="subtext">Перезапуск: ${c.restart === "passed" ? "проверен" : c.restart === "failed" ? "ошибка" : "не проверен"}</span></td></tr>`;
  }).join("");
}

function renderActivity() {
  const filter = $("strategy-filter").value, selected = selectedCase();
  const accounts = scopedAccounts();
  $("activity-scope").textContent = filter ? `${filter} · счёт ${selected.account_id.slice(0,8)}` : "Все счета кампании";
  let rows = [], headers = [], note = "", title = "Пока нет заявок", description = "Здесь появятся заявки после естественного сигнала стратегии.";
  if (activeTab === "orders") {
    headers = ["Время, МСК","Стратегия / счёт","Инструмент","Направление","Лоты / исполнено","Статус","Назначение"];
    const orders = data.orders.filter(o => !filter || o.strategy === filter).map(o => ({...o,fromApi:false}));
    const ids = new Set(data.orders.map(o => o.exchange_order_id).filter(Boolean));
    for (const account of accounts) {
      if (selected && !selected.owns_account) continue;
      for (const o of account.active_orders) if (!ids.has(o.id)) orders.push({created_at:o.time,strategy:account.id.slice(0,8),instrument:o.instrument,side:o.side,lots:o.lots,lots_executed:o.executed,status:o.status,fromApi:true});
    }
    orders.sort((a,b) => String(b.created_at || "").localeCompare(a.created_at || ""));
    rows = orders.map(o => `<tr><td>${esc(datetime(o.created_at))}</td><td>${esc(o.strategy)}</td><td>${esc(o.instrument)}</td><td>${side(o.side)}</td><td>${fmt(o.lots)} / ${fmt(o.lots_executed)}</td><td>${badge(status(o.status),/filled|fill$/.test(o.status) ? "" : /failed|reject/.test(o.status) ? "error" : "waiting")}</td><td>${o.fromApi ? badge("Активная в API") : o.maintenance ? badge("Служебное закрытие","warn") : o.manual ? badge("Ручная заявка","warn") : "Стратегия"}</td></tr>`);
  } else if (activeTab === "fills") {
    headers = ["Время, МСК","Стратегия","Инструмент","Направление","Лоты","Цена","Назначение"];
    const fills = data.fills.filter(f => !filter || f.strategy === filter).sort((a,b) => String(b.executed_at).localeCompare(a.executed_at));
    rows = fills.map(f => `<tr><td>${esc(datetime(f.executed_at))}</td><td>${esc(f.strategy || "Не сопоставлена")}</td><td>${esc(f.instrument || "—")}</td><td>${side(f.side)}</td><td>${fmt(f.lots)}</td><td>${fmt(f.price,2)}</td><td>${f.maintenance ? badge("Служебное закрытие","warn") : f.strategy ? "Стратегия" : "Не сопоставлено"}</td></tr>`);
    title = "В журнале пока нет исполнений";description = "Фактические движения по счетам также доступны во вкладке «Операции брокера».";
  } else if (activeTab === "positions") {
    headers = ["Счёт / текущая стратегия","Инструмент","Количество","Лоты","Текущая цена","Валюта"];
    for (const account of accounts) {
      if (selected && !selected.owns_account) continue;
      rows.push(...account.positions.map(p => `<tr><td>${esc(account.id.slice(0,8))}<span class="subtext">${esc(account.owner || "Владелец не определён")}</span></td><td>${esc(p.instrument)}</td><td>${fmt(p.quantity,4)}</td><td>${fmt(p.lots,4)}</td><td>${fmt(p.price,2)}</td><td>${esc((p.currency || "").toUpperCase())}</td></tr>`));
    }
    title = accounts.every(a => a.available) ? "Открытых позиций нет" : "Снимок позиций недоступен";
    description = selected && !selected.owns_account ? "Этот сценарий сейчас не владеет счётом. Чужие позиции ему не приписываются." : "Рублёвый остаток показан в карточках счетов ниже.";
  } else {
    headers = ["Время, МСК","Счёт","Операция","Инструмент","Сумма","Комиссия","Статус"];
    const operations = accounts.flatMap(a => a.operations.map(o => ({...o,account:a.id}))).sort((a,b) => String(b.time).localeCompare(a.time));
    rows = operations.map(o => {
      const type = /INPUT/.test(o.type) ? "Пополнение" : /OUTPUT/.test(o.type) ? "Вывод" : /BUY/.test(o.type) ? "Покупка" : /SELL/.test(o.type) ? "Продажа" : /FEE|COMMISS/.test(o.type) ? "Комиссия" : o.type;
      return `<tr><td>${esc(datetime(o.time))}</td><td class="mono">${esc(o.account.slice(0,8))}</td><td>${esc(type)}</td><td>${esc(o.instrument || o.name || "—")}</td><td>${fmt(o.payment,2)} ${esc((o.currency || "").toUpperCase())}</td><td>${fmt(o.commission,2)}</td><td>${badge(o.status === "OPERATION_STATE_EXECUTED" ? "Проведена" : o.status)}</td></tr>`;
    });
    title = "Операций пока нет";description = "Пополнения, сделки и комиссии появятся после обновления снимка брокера.";
    note = "Полная история выбранных счетов. При переиспользовании счёта сюда входят операции всех его стратегий; пополнение не является торговым исполнением.";
  }
  if (!data.updated.broker && ["positions","operations"].includes(activeTab)) {title = "Ожидаем данные брокера";description = "Снимок ещё не доступен. Страница обновится автоматически.";}
  $("activity-content").innerHTML = (note ? `<div class="section-inline-note">${esc(note)}</div>` : "") + (rows.length ? table(headers,rows) : empty(title,description));
}

function renderAccounts() {
  const selected = selectedCase();
  $("account-cards").innerHTML = data.accounts.map((a,index) => `<article class="account-card ${selected && a.id !== selected.account_id ? "dim" : ""}"><div class="account-card-top"><span class="eyebrow">СЧЁТ ${String(index+1).padStart(2,"0")}</span><span class="account-id" title="${esc(a.id)}">${esc(a.id.slice(0,8))}</span></div><div class="account-equity">${fmt(a.equity)} ₽</div><div class="account-owner">${esc(a.owner || "Владелец ещё не назначен")}</div><span class="subtext">Рублёвый остаток: ${fmt(a.cash)} ₽</span><div class="account-bottom"><span>${a.available ? a.positions.length : "—"} позиций</span><span>${a.available ? a.active_orders.length : "—"} заявок</span><span>${a.cases.length} ${a.cases.length === 1 ? "сценарий" : "сценария"}</span></div>${!a.available ? '<span class="subtext warning">Снимок счёта недоступен</span>' : ""}</article>`).join("");
}

function renderChart() {
  const account = $("chart-account").value;
  chartPoints = data.history.map(point => ({time:point.time,value:account ? point.accounts[account] : point.complete ? Object.values(point.accounts).reduce((a,b) => a+b,0) : null})).filter(p => p.value != null && Number.isFinite(p.value));
  const svg = $("equity-chart");
  $("chart-empty").hidden = chartPoints.length >= 2;
  $("chart-tooltip").hidden = true;
  if (chartPoints.length < 2) {svg.replaceChildren();return;}
  const values = chartPoints.map(p => p.value), low = Math.min(...values), high = Math.max(...values);
  const pad = Math.max((high-low)*.2,high*.0002,1), min = low-pad, max = high+pad;
  const first = Date.parse(chartPoints[0].time), last = Date.parse(chartPoints.at(-1).time);
  const x = p => 95+(Date.parse(p.time)-first)/Math.max(last-first,1)*880;
  const y = p => 178-(p.value-min)/(max-min)*157;
  let grid = "";
  for (let i=0;i<4;i++) {const yy=21+i*157/3;grid+=`<line x1="95" x2="975" y1="${yy}" y2="${yy}" class="chart-grid"/><text x="80" y="${yy+4}" text-anchor="end" class="chart-label">${esc(fmt(max-i*(max-min)/3))}</text>`;}
  const points = chartPoints.map(p => `${x(p).toFixed(2)},${y(p).toFixed(2)}`).join(" ");
  svg.innerHTML = `${grid}<polygon points="95,178 ${points} 975,178" class="chart-area"/><polyline points="${points}" class="chart-line"/><circle cx="${x(chartPoints.at(-1))}" cy="${y(chartPoints.at(-1))}" r="4" class="chart-dot"/><text x="95" y="207" class="chart-label">${esc(datetime(chartPoints[0].time))}</text><text x="975" y="207" text-anchor="end" class="chart-label">${esc(datetime(chartPoints.at(-1).time))} МСК</text>`;
}

async function refresh() {
  if (busy) return;
  busy = true;$("refresh").disabled = true;
  try {
    const response = await fetch("/api/snapshot", {cache:"no-store",signal:AbortSignal.timeout(8000)});
    if (!response.ok) throw new Error("snapshot unavailable");
    const next = await response.json();
    if (!next || !Array.isArray(next.cases) || !Array.isArray(next.accounts)) throw new Error("invalid snapshot");
    data = next;render();
  } catch (_) {
    $("connection").textContent = "Нет связи со страницей";$("connection").className = "badge error";
    $("notice").hidden = false;$("notice").textContent = "Локальный сервер недоступен. Ниже сохранён последний полученный снимок; его актуальность не подтверждена.";
  } finally {busy = false;$("refresh").disabled = false;}
}

$("refresh").addEventListener("click", refresh);
$("strategy-filter").addEventListener("change", () => {if(data){renderStrategies();renderActivity();renderAccounts();renderDetail();}});
$("chart-account").addEventListener("change", () => {if(data)renderChart();});
document.querySelectorAll("[data-tab]").forEach(button => button.addEventListener("click", () => {
  activeTab = button.dataset.tab;
  document.querySelectorAll("[data-tab]").forEach(b => b.setAttribute("aria-selected", String(b === button)));
  $("activity-content").setAttribute("aria-labelledby",button.id);
  if(data)renderActivity();
}));
document.querySelectorAll("nav a").forEach(link => link.addEventListener("click", () => {
  document.querySelectorAll("nav a").forEach(a => a.classList.toggle("selected",a === link));
}));
$("equity-chart").addEventListener("pointermove", event => {
  if (chartPoints.length < 2) return;
  const proportion = Math.max(0,Math.min(1,(svgX(event)-95)/880));
  const first = Date.parse(chartPoints[0].time), target = first+proportion*(Date.parse(chartPoints.at(-1).time)-first);
  const point = chartPoints.reduce((best,p) => Math.abs(Date.parse(p.time)-target)<Math.abs(Date.parse(best.time)-target) ? p : best);
  $("chart-tooltip").textContent = `${datetime(point.time)} МСК · ${fmt(point.value,2)} ₽`;$("chart-tooltip").hidden = false;
});
$("equity-chart").addEventListener("pointerleave", () => {$("chart-tooltip").hidden = true;});
setInterval(() => {if(!document.hidden)refresh();},10000);
document.addEventListener("visibilitychange", () => {if(!document.hidden)refresh();});
refresh();
