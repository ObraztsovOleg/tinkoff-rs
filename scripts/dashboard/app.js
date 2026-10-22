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
const statusNames = {"real token not connected":"Нет токена реального счёта","live trading disabled":"Нет токена реального счёта","emergency stop":"Аварийная остановка","stopped; real positions kept":"Остановлена, позиции сохранены",prepared:"Ожидает сессию",running:"Запущена",cleaned:"Завершена",cleanup_pending:"Закрытие позиций","waiting for predecessor cleanup":"Ожидает счёт","failed; coverage incomplete":"Запуск не удался","restart backoff":"Пауза перед перезапуском","paused; risk monitor":"Служебный процесс",paused:"Выключена","waiting for runtime slot":"Ждёт свободного места","waiting for account owner":"Ждёт счёт",filled:"Исполнена",partially_filled:"Частично",pending:"Подготовлена",submitted:"Отправлена",cancelled:"Отменена",canceled:"Отменена",rejected:"Отклонена",failed:"Ошибка",new:"Новая"};
const status = value => {
  const normalized = String(value || "").toLowerCase().replace("execution_report_status_", "");
  return statusNames[normalized] || value || "—";
};
let data = null, activeTab = "orders", busy = false, chartPoints = [], strategiesArchive = false;
const time = value => date(value, {day: undefined, month: undefined, hour: "2-digit", minute: "2-digit"});
const weekday = value => new Intl.DateTimeFormat("ru-RU", {timeZone: "Europe/Moscow", weekday: "short", day: "numeric", month: "short"}).format(new Date(value));

function duration(milliseconds) {
  const minutes = Math.max(0, Math.round(milliseconds / 60000)), days = Math.floor(minutes / 1440), hours = Math.floor(minutes % 1440 / 60);
  return days ? `${days} дн ${hours} ч` : hours ? `${hours} ч ${minutes % 60} мин` : `${minutes} мин`;
}

// Trading sessions of the MOEX calendar: the current one, else the next one.
function tradingSessions() {
  return (data?.campaign?.calendar || []).filter(d => d.isTradingDay && d.startTime && d.endTime);
}
function nearestSession() {
  const now = Date.now();
  return tradingSessions().find(d => Date.parse(d.endTime) > now);
}

function dayRange(days) {
  if (!days?.length) return "";
  const first = date(days[0] + "T12:00:00Z"), last = date(days.at(-1) + "T12:00:00Z");
  return days.length === 1 ? first : `${first} – ${last}`;
}
// Operator schedule of one account, as computed by the supervisor.
function scheduleSummary(c) {
  const schedule = c.schedule || {};
  if (schedule.run_mode === "always") return "Постоянно, пока включена";
  const windows = (schedule.windows || []).filter(w => Date.parse(w.end) > Date.now());
  if (!windows.length) return "Дни не выбраны";
  return windows.slice(0, 3).map(w => dayRange(w.days)).join(", ") + (windows.length > 3 ? ` и ещё ${windows.length - 3}` : "");
}
const displayName = c => c.title || c.kind;
// Russian plural: plural(2, ["счёт", "счёта", "счетов"]) → "счёта".
const plural = (n, [one, few, many]) => {
  const tens = Math.abs(n) % 100, units = tens % 10;
  return tens > 10 && tens < 20 ? many : units === 1 ? one : units >= 2 && units <= 4 ? few : many;
};

// The supervisor records when the computer (and the whole stand) was asleep.
// A sleep that overlapped a trading session in the last 36 hours is a missed session.
function sleepWarnings() {
  const recent = Date.now() - 36 * 3600000;
  return (data?.campaign?.suspensions || []).filter(s => Date.parse(s.to) > recent).flatMap(s => {
    const from = Date.parse(s.from), to = Date.parse(s.to);
    const missed = tradingSessions().filter(d => Date.parse(d.startTime) < to && Date.parse(d.endTime) > from);
    return missed.length ? [`Компьютер спал ${datetime(s.from)} – ${datetime(s.to)} МСК: стенд стоял, торговая сессия ${missed.map(d => date(d.date + "T12:00:00Z")).join(", ")} пропущена${Date.parse(missed[0].startTime) >= from && Date.parse(missed.at(-1).endTime) <= to ? "" : " частично"}. Во время торгов держите Mac включённым от сети и не закрывайте крышку.`] : [];
  });
}

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
  // Portfolios by contour; the campaign identifier stays in the tooltip.
  const current = cases.filter(c => typeof isCurrentCase !== "function" || isCurrentCase(c));
  const realCount = current.filter(c => c.contour === "production").length;
  $("campaign-id").textContent = campaign.id ? `Портфелей: ${current.length} · в песочнице ${current.length - realCount} · на реальных счетах ${realCount}` : "Стенд не подготовлен";
  $("campaign-id").title = campaign.id || "";
  const observing = campaign.supervisor_running;
  $("connection").textContent = observing ? "● Наблюдатель работает" : campaign.phase === "complete" ? "Наблюдение завершено" : "Наблюдатель остановлен";
  $("connection").className = `badge ${observing ? "" : "warn"}`;
  const warnings = [...data.warnings];
  if (age(data.updated.broker) > 600) warnings.push("Данные брокера старше 10 минут или отсутствуют. Значения ниже могут быть неактуальны.");
  if (age(data.updated.database) > 180) warnings.push("Отчёт стратегий не обновлялся более трёх минут.");
  warnings.push(...sleepWarnings());
  if (typeof storageWarnings === "function") warnings.push(...storageWarnings());
  $("notice").hidden = warnings.length === 0;
  $("notice").textContent = warnings.join(" ");
  const session = nearestSession(), live = session && Date.parse(session.startTime) <= Date.now();
  $("session-hint").className = `session-hint${live ? " live" : ""}`;
  $("session-hint").textContent = !session ? "Календарь торговых дней ещё не получен от T-Invest." : live
    ? `Сейчас идёт торговая сессия: ${time(session.startTime)}–${time(session.endTime)} МСК, до закрытия ${duration(Date.parse(session.endTime) - Date.now())}.`
    : `Ближайшая торговая сессия: ${weekday(session.startTime)}, ${time(session.startTime)}–${time(session.endTime)} МСК · через ${duration(Date.parse(session.startTime) - Date.now())}.`;
  $("next-session").textContent = `${tradingSessions().filter(d => Date.parse(d.endTime) > Date.now()).length} торговых дней в календаре`;
  // Virtual and real rubles are never added together.
  const virtual = accounts.filter(a => a.contour !== "production"), real = accounts.filter(a => a.contour === "production");
  const known = virtual.filter(a => a.equity != null);
  $("total-equity").textContent = known.length === virtual.length && virtual.length ? fmt(known.reduce((sum,a) => sum+a.equity,0)) : "—";
  $("account-count").textContent = `${virtual.length} ${plural(virtual.length, ["счёт", "счёта", "счетов"])} песочницы · виртуальные ₽${known.length < virtual.length ? ` · доступно ${known.length}` : ""}${real.length ? ` · ${plural(real.length, ["реальный счёт", "реальных счёта", "реальных счетов"])}: ${real.length} (${fmt(real.reduce((sum,a) => sum+(a.equity || 0),0))} ₽, отдельно)` : ""}`;
  $("strategy-count").textContent = current.length;
  const service = cases.filter(c => c.running && c.service_only).length;
  $("runtime-count").textContent = `${cases.filter(c => c.running && !c.service_only).length} торгуют${service ? ` · ${service} служебных` : ""} · ${cases.filter(c => c.preflight_result === "passed" || c.readiness_observed).length} прогреты`;
  const upcoming = cases.filter(c => c.launch_at && !c.running && Date.parse(c.launch_at) > Date.now()).sort((a,b) => Date.parse(a.launch_at)-Date.parse(b.launch_at));
  if (!cases.some(c => c.running && !c.service_only) && upcoming.length) $("runtime-count").textContent = `Ближайший запуск по расписанию: ${datetime(upcoming[0].launch_at)}`;
  $("open-orders").textContent = accounts.every(a => a.available) && accounts.length ? accounts.reduce((sum,a) => sum+a.active_orders.length,0) : "—";
  const filled = data.orders.filter(o => o.status === "filled");
  $("filled-orders").textContent = data.updated.database ? filled.filter(o => !o.maintenance).length : "—";
  $("filled-note").textContent = `Стратегии · служебных закрытий: ${filled.filter(o => o.maintenance).length}`;
  $("strategies-total").textContent = current.length;
  const archived = cases.filter(c => !current.includes(c));
  $("strategies-archive").textContent = strategiesArchive ? "← Текущие" : `Архив (${archived.length})`;
  $("strategies-archive").hidden = !archived.length && !strategiesArchive;
  options("strategy-filter", [...current, ...archived].map(c => [c.kind, displayName(c) + (current.includes(c) ? "" : " · архив")]), "Все стратегии");
  options("activity-filter", cases.map(c => [c.kind,displayName(c)]), "Все стратегии");
  $("activity-filter").value = $("strategy-filter").value;
  options("chart-account", accounts.map(a => [a.id,`${a.id.slice(0,8)} · ${a.owner || a.cases[0] || "счёт"}`]), "Все счета");
  renderStrategies(); renderActivity(); renderAccounts(); renderChart();
  if (typeof renderPortfolios === "function") renderPortfolios();
  if (typeof renderAiBoard === "function") renderAiBoard();
  renderDetail();
  if (typeof renderStorage === "function") renderStorage();
  // The address may name a strategy whose data arrives only now.
  if (typeof applyView === "function") applyView();
  $("db-updated").textContent = `Отчёт стратегий: ${datetime(data.updated.database)} МСК · ${ageText(data.updated.database)}`;
  $("broker-updated").textContent = `Брокер: ${datetime(data.updated.broker)} МСК · ${ageText(data.updated.broker)}`;
}

// What a portfolio process does now, as text and badge type.
function caseState(c) {
  let state = status(c.state), type = "waiting";
  if (c.running && c.service_only) {state = "Служебный процесс";type = "warn";}
  else if (c.running) {state = "Торгует";type = "";}
  else if (c.state === "running") {state = "Процесс остановлен";type = "warn";}
  // The supervisor keeps the last process state; without a live process it is only a schedule wait.
  else if (c.state === "paused; risk monitor") state = Date.parse(c.launch_at) > Date.now() ? "Ожидает сессию" : "Не запущена";
  if (/failed|pending/.test(c.state)) type = "warn";
  return [state, type];
}

function renderStrategies() {
  const filter = $("strategy-filter").value;
  // Old scenarios that gave their accounts away are listed only in the archive.
  const archived = c => typeof isCurrentCase === "function" && !isCurrentCase(c);
  $("strategy-rows").innerHTML = data.cases.filter(c => filter ? c.kind === filter : archived(c) === strategiesArchive).map(c => {
    const ready = c.preflight_result === "passed" || c.readiness_observed;
    const [state, type] = caseState(c);
    const zero = Math.max(0,(c.natural_target_count || 0)-(c.nonzero_target_count || 0));
    const riskText = c.risk.blocked ? '<span class="badge error">Риск: блокировка</span>' : c.risk.saved ? '<span class="good">Риск сохранён</span>' : '<span class="muted-text">Риск: ожидает</span>';
    return `<tr data-strategy="${esc(c.kind)}" class="${c.contour === "production" ? "live-row" : ""}"><td><button type="button" class="strategy-link" data-strategy="${esc(c.kind)}">${esc(displayName(c))}</button>${c.contour === "production" ? ' <span class="badge live">РЕАЛЬНЫЙ</span>' : ""}<span class="subtext">${esc(c.account_id.slice(0,8))}${c.title && c.title !== c.kind ? " · " + esc(c.kind) : ""}${c.blocks?.length ? ` · блоков: ${c.blocks.length}` : ""}${c.predecessor && !c.owns_account ? " · счёт после " + esc(c.predecessor) : ""}</span></td><td>${esc(scheduleSummary(c))}${c.schedule?.active ? '<span class="subtext good">Окно идёт</span>' : ""}</td><td>${badge(state,type)}${c.running && c.service_only ? '<span class="subtext">Без автосигналов</span>' : ""}${(!c.running || c.service_only) && c.launch_at && c.state !== "cleaned" && Date.parse(c.launch_at) > Date.now() ? `<span class="subtext">Запуск ${esc(datetime(c.launch_at))} МСК</span>` : ""}${c.failed_batches ? `<span class="subtext warning">Ошибок пакетов: ${fmt(c.failed_batches)}</span>` : ""}</td><td>${ready ? '<span class="good">✓ Подтверждён</span>' : '<span class="muted-text">Ожидает</span>'}</td><td class="num">${fmt(c.natural_decision_count || 0)} / ${fmt(c.natural_target_count || 0)}<span class="subtext">нулевых целей: ${fmt(zero)}</span></td><td class="num">${fmt(c.strategy_filled_intents || 0)}</td><td>${riskText}<span class="subtext">Перезапуск: ${c.restart === "passed" ? "проверен" : c.restart === "failed" ? "ошибка" : "не проверен"}</span></td></tr>`;
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
    headers = ["Время, МСК","Стратегия","Инструмент","Направление","Лоты","Цена","Назначение",""];
    const fills = data.fills.filter(f => !filter || f.strategy === filter).sort((a,b) => String(b.executed_at).localeCompare(a.executed_at));
    rows = fills.map(f => `<tr><td>${esc(datetime(f.executed_at))}</td><td>${esc(f.strategy || "Не сопоставлена")}</td><td>${esc(f.instrument || "—")}</td><td>${side(f.side)}</td><td>${fmt(f.lots)}</td><td>${fmt(f.price,2)}</td><td>${f.maintenance ? badge("Служебное закрытие","warn") : f.strategy ? "Стратегия" : "Не сопоставлено"}</td><td>${f.strategy && f.instrument_id ? `<button type="button" class="button small" data-trade-strategy="${esc(f.strategy)}" data-trade-instrument="${esc(f.instrument_id)}" data-trade-key="${esc(tradeKey(f))}">На графике</button>` : ""}</td></tr>`);
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
  $("account-cards").innerHTML = data.accounts.map((a,index) => {
    const portfolios = data.cases.filter(c => c.account_id === a.id);
    const composition = portfolios.map(c => `<button type="button" class="account-portfolio" data-strategy="${esc(c.kind)}"><strong>${esc(displayName(c))}</strong><span>${esc(c.blocks?.length ? c.blocks.map(b => `${b.strategies.map(x => x.kind).join(" + ")} · ${b.instruments.map(i => i.ticker).join(", ")} · ${b.share}%`).join("; ") : (c.assets || []).filter(x => !x.benchmark).map(x => x.ticker).join(", "))}</span><span>${esc(scheduleSummary(c))}</span></button>`).join("");
    return `<article class="account-card ${a.contour === "production" ? "live" : ""} ${selected && a.id !== selected.account_id ? "dim" : ""}"><div class="account-card-top"><span class="eyebrow">${a.contour === "production" ? "РЕАЛЬНЫЙ СЧЁТ" : "СЧЁТ " + String(index+1).padStart(2,"0")}</span><span class="account-id" title="${esc(a.id)}">${esc(a.id.slice(0,8))}</span></div><div class="account-equity">${fmt(a.equity)} ₽</div><div class="account-owner">${esc(a.owner ? "Торгует: " + a.owner : "Владелец ещё не назначен")}</div>${composition}<span class="subtext">Рублёвый остаток: ${fmt(a.cash)} ₽</span><div class="account-bottom"><span>${a.available ? a.positions.length : "—"} позиций</span><span>${a.available ? a.active_orders.length : "—"} заявок</span><span>${a.cases.length} ${a.cases.length === 1 ? "сценарий" : "сценария"}</span></div>${!a.available ? '<span class="subtext warning">Снимок счёта недоступен</span>' : ""}</article>`;
  }).join("");
}

function renderChart() {
  const account = $("chart-account").value;
  const real = new Set(data.accounts.filter(a => a.contour === "production").map(a => a.id));
  chartPoints = data.history.map(point => ({time:point.time,value:account ? point.accounts[account] : point.complete ? Object.entries(point.accounts).filter(([id]) => !real.has(id)).reduce((a,[,b]) => a+b,0) : null})).filter(p => p.value != null && Number.isFinite(p.value));
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
$("strategies-archive").addEventListener("click", () => {strategiesArchive = !strategiesArchive; if (data) render();});
$("account-cards").addEventListener("click", event => {
  const button = event.target.closest("[data-strategy]");
  if (button) chooseStrategy(button.dataset.strategy);
});
$("activity-content").addEventListener("click", event => {
  const button = event.target.closest("[data-trade-key]");
  if (button) showTradeOnChart(button.dataset.tradeStrategy, button.dataset.tradeInstrument, button.dataset.tradeKey);
});
$("strategy-filter").addEventListener("change", () => {if(data)chooseStrategy($("strategy-filter").value);});
// Trades and positions of one strategy, without leaving the section.
$("activity-filter").addEventListener("change", () => {if(data){$("strategy-filter").value = $("activity-filter").value;renderStrategies();renderActivity();renderAccounts();renderDetail();}});
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
