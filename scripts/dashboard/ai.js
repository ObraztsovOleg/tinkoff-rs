"use strict";

// AI model strategy: the model profiles (#models) and decisions on a strategy page.
// Connections live in the profiles file; the page never receives a token.
const AI_KINDS={decision:"Решение",preview:"Пробный анализ"};
const AI_TRIGGERS={session_open:"открытие сессии",scheduled:"плановый пересмотр",news_burst:"всплеск новостей",price_jump:"скачок цены",material_event:"важное событие эмитента",macro_shock:"движение рынка",thesis_condition:"условие тезиса",preview:"пробный анализ"};
const AI_TOKEN=token=>token==="file" ? "задан в файле" : token?.startsWith("env:") ? `из переменной ${token.slice(4)}` : "не задан";
const isAiCase=c=>Boolean(c?.ai_model) || (c?.blocks || []).some(b=>b.strategies.some(s=>s.kind==="ai-model"));
const safeLink=url=>/^https?:\/\//.test(url || "") ? url : null;
const AI_PURPOSES={decision:"решения",preview:"пробные анализы",tagging:"разметка документов",embeddings:"векторы",reflection:"разбор итогов",exposures:"профили бумаг",facts:"цифры отчётов"};
const rub=value=>value==null || !Number.isFinite(Number(value)) ? "—" : `${fmt(Number(value),Number(value)<10 ? 2 : 0)} ₽`;
// A model price in its own currency; dollars also in rubles at the exchange rate.
const modelPrice=(value,currency)=>{
  if(value==null)return "—";
  if(currency==="rub")return `${fmt(value,2)} ₽`;
  const rate=Number(controlsData?.usd_rub?.rub || 0);
  return `${fmt(value,3)} $${rate ? ` ≈ ${fmt(value*rate,value*rate<10 ? 2 : 0)} ₽` : ""}`;
};
const tokens=value=>value>=1e6 ? `${fmt(value/1e6,1)} млн` : value>=1e3 ? `${fmt(value/1e3,1)} тыс.` : fmt(value);
// Known model errors in operator language.
const aiError=text=>{
  const budget=/^daily AI budget exhausted: ([\d.]+) of ([\d.]+) USD/.exec(text || "");
  if(budget)return `Дневной лимит расходов на ИИ исчерпан ($${budget[1]} из $${budget[2]}): внеочередные решения возобновятся завтра, плановые продолжаются.`;
  const rubBudget=/^daily AI budget exhausted: ([\d.]+) of ([\d.]+) RUB/.exec(text || "");
  if(rubBudget)return `Дневной лимит расходов на ИИ исчерпан (${rubBudget[1]} ₽ из ${rubBudget[2]} ₽): внеочередные решения возобновятся завтра, плановые продолжаются.`;
  if(/credit balance is too low/i.test(text || ""))return "На счёте Anthropic закончились средства: пополните баланс в console.anthropic.com → Plans & Billing. До пополнения модель не принимает решений, позиции не меняются.";
  if(/HTTP 401/.test(text || ""))return "Провайдер отклонил ключ модели (HTTP 401): проверьте token в файле профилей.";
  if(/HTTP 429/.test(text || ""))return "Провайдер ограничил частоту запросов (HTTP 429): решение повторится позже.";
  if(/has no token/.test(text || ""))return "В профиле модели нет ключа: задайте token в файле профилей.";
  return text;
};
// Today's spend against the daily limit (Moscow day), the last seven days by purpose.
// Spend on model requests in rubles: today against the daily limit, the last seven
// days by purpose. Prices in dollars are converted at the exchange USD/RUB rate.
function aiSpendHtml() {
  const rows=controlsData?.ai_usage || [], limit=Number(controlsData?.ai_budget_rub || 0), rate=controlsData?.usd_rub;
  const today=new Intl.DateTimeFormat("en-CA",{timeZone:"Europe/Moscow"}).format(new Date());
  const sum=(list,key)=>list.reduce((total,r)=>total+Number(r[key] || 0),0);
  const todays=rows.filter(r=>r.day===today), week=rows;
  const spent=sum(todays,"cost_rub"), unpriced=sum(week,"unpriced");
  const parts=Object.entries(Object.groupBy ? Object.groupBy(week,r=>r.purpose) : week.reduce((g,r)=>((g[r.purpose] ||= []).push(r),g),{}))
    .map(([purpose,list])=>`${esc(AI_PURPOSES[purpose] || purpose)} ${rub(sum(list,"cost_rub"))}`).join(" · ");
  const inFile=controlsData?.ai_models?.budget_in_file;
  return `<div class="ai-spend"><span>Сегодня: <strong class="${limit && spent>=limit ? "warning" : ""}">${rub(spent)}</strong> из дневного лимита ${rub(limit)}${limit && spent>=limit ? " — лимит исчерпан до полуночи по Москве" : ""}</span><span>За 7 дней: <strong>${rub(sum(week,"cost_rub"))}</strong>${parts ? ` (${parts})` : ""}</span><span class="muted-text">Токены за 7 дней: ${tokens(sum(week,"input_tokens"))} на входе (из кэша ${tokens(sum(week,"cache_read_tokens"))}), ${tokens(sum(week,"output_tokens"))} на выходе</span><span class="muted-text">${rate ? `Цены в долларах пересчитываются по курсу ${fmt(Number(rate.rub),2)} ₽ за $ (Мосбиржа, ${datetime(rate.observed_at)} МСК)` : "Курс доллара ещё не получен: расходы в долларах оцениваются по 100 ₽ за $"}</span>${unpriced ? `<span class="warning">Без цен в профиле: ${fmt(unpriced)} ${plural(unpriced,["работа","работы","работ"])} не оценены</span>` : ""}${inFile ? `<span class="warning">В файле профилей остался раздел [budget] (${fmt(inFile,2)} $ в сутки) — он больше не действует, лимит задаётся здесь в рублях</span>` : ""}</div>`;
}

$("instrument-catalog").insertAdjacentHTML("afterend", `<section class="panel" id="models"><div class="section-heading"><div><h2>Модели ИИ</h2><p id="models-meta" class="muted">Подключения для стратегии «ИИ-модель»</p></div></div><div id="ai-board"></div><div class="ai-costs"><h3>Расходы на запросы к моделям</h3><p class="detail-note">Решения, разметка документов, разборы и профили бумаг оплачиваются провайдеру модели. Дневной лимит — сколько стенд может потратить за сутки по Москве: после него модель не спрашивают вне расписания, документы не размечаются, разборы ждут следующего дня; плановые решения продолжаются.</p><div id="ai-spend"></div><form id="ai-budget-form" class="inline-form"><label for="ai-budget">Дневной лимит, ₽</label><input id="ai-budget" type="number" min="1" max="1000000" step="1" required><button class="button" type="submit">Сохранить</button></form></div><div id="models-list"></div><div class="models-help"><h3>Как подключить модель</h3><ol><li>Откройте файл профилей <code id="models-path">docker/local/ai/models.toml</code> (создаётся командой <code>python3 scripts/compose-setup.py init</code>).</li><li>Для каждой модели задайте <code>provider</code> (<code>anthropic</code> или <code>openai</code> — любой OpenAI-совместимый API), <code>url</code>, <code>model</code> и <code>token</code> либо <code>token_env</code>.</li><li>Сохраните файл: стенд перечитывает его при каждом решении. Нажмите «Проверить».</li><li>В «Стратегии → Собрать портфель счёта» добавьте блок «ИИ-модель» и выберите профиль.</li></ol></div></section>`);
$("strategy-log").insertAdjacentHTML("beforebegin", `<div id="ai-panel" class="ai-panel" hidden><div class="log-heading"><div><h3>Решения модели <span id="ai-count" class="count-pill">0</span></h3><p class="detail-note">Модель решает на открытии, по расписанию и внеочередно — при всплеске новостей или резком движении цены. Решение может быть любым, в том числе «держать». Лимиты стратегии и риск-менеджер применяются после неё.</p></div><button id="ai-preview" class="button" type="button">Пробный анализ</button></div><div id="ai-decisions"></div><h3 class="ai-memory-title">Тезисы <span id="ai-theses-count" class="count-pill">0</span></h3><p class="detail-note">Идеи модели по бумагам: направление, обоснование и условия выхода. Когда условие наступает, стенд спрашивает модель внеочередно; при закрытии считается результат от цены открытия.</p><div id="ai-theses"></div><h3 class="ai-memory-title">Память модели <span id="ai-memory-count" class="count-pill">0</span></h3><p class="detail-note">Долгосрочные заметки, которые модель сама сохраняет и закрывает: тезисы по бумагам, планы, условия выхода.</p><div id="ai-memory"></div></div>`);

function aiDecisionHtml(d) {
  const tools=d.tool_calls?.calls || [], errors=d.tool_calls?.feed_errors || [];
  const weights=(d.weights || []).map(w=>`<span class="ai-weight ${Number(w.weight_percent)>0 ? "long" : Number(w.weight_percent)<0 ? "short" : ""}">${esc(w.ticker)} ${fmt(w.weight_percent,2)}%</span>`).join("") || '<span class="muted-text">Без позиций</span>';
  // Documents of the decision: cited ones first; stored documents open in the Documents section.
  const sources=(d.sources || []).slice(0,15).map(s=>{const link=s.id ? `#documents/${encodeURIComponent(s.id)}` : safeLink(s.link);const external=!s.id && link;return `<li class="${s.cited ? "cited" : ""}">${s.cited ? '<span class="ai-cited">основание</span> ' : ""}${link ? `<a href="${esc(link)}"${external ? ' target="_blank" rel="noopener noreferrer"' : ""}>${esc(s.title)}</a>` : esc(s.title)} <span class="subtext">${esc(typeof docSource==="function" ? (s.sources || [s.source]).filter(Boolean).map(docSource).join(", ") : s.source || "")} · ${esc(String(s.published || "").includes("T") ? datetime(s.published) : s.published || "")}${s.read ? " · прочитан полностью" : ""}</span></li>`;}).join("");
  const toolList=tools.map(t=>`${esc(t.tool)}${t.error ? " ✕" : ""}`).join(", ");
  const outcomes=Object.entries(d.outcome || {}).sort(([a],[b])=>parseInt(a)-parseInt(b)).map(([h,o])=>`через ${parseInt(h)} ${plural(parseInt(h),["день","дня","дней"])}: ${o.portfolio==null ? "—" : `${o.portfolio>0 ? "+" : ""}${fmt(o.portfolio,2)}%`} против индекса ${o.imoex>0 ? "+" : ""}${fmt(o.imoex,2)}%`).join(" · ");
  const spend=d.usage?.requests ? ` · ${fmt(d.usage.requests)} ${plural(d.usage.requests,["запрос","запроса","запросов"])}, ${tokens((d.usage.input_tokens || 0)+(d.usage.cache_read_tokens || 0)+(d.usage.cache_write_tokens || 0))} → ${tokens(d.usage.output_tokens || 0)} токенов${d.cost_rub!=null ? `, ${rub(Number(d.cost_rub))}` : ""}` : "";
  const trigger=d.trigger && d.kind!=="preview" ? `<span class="ai-trigger ${/burst|jump|material|macro|thesis/.test(d.trigger) ? "extra" : ""}">${esc(AI_TRIGGERS[d.trigger] || d.trigger)}</span>` : "";
  const held=d.hold ? '<span class="ai-weight">Держать: без сделок</span>' : weights;
  return `<details class="ai-decision ${d.status==="failed" ? "failed" : ""}"><summary><span>${esc(datetime(d.created_at))} МСК</span>${badge(AI_KINDS[d.kind] || d.kind,d.kind==="preview" ? "waiting" : "")}${trigger}${d.status==="failed" ? badge("Ошибка","error") : ""}<span class="ai-model">${esc(d.model || d.profile || "модель")}</span><span class="ai-weights">${d.status==="failed" ? esc((aiError(d.error) || "").slice(0,160)) : held}</span>${d.cost_rub!=null ? `<span class="ai-cost">${rub(Number(d.cost_rub))}</span>` : ""}</summary>
    ${d.tool_calls?.details ? `<p class="detail-note">Повод: ${esc(d.tool_calls.details)}</p>` : ""}
    ${d.status==="failed" ? `<p class="warning">${esc(aiError(d.error) || "Ошибка без описания")}</p><p class="detail-note">Решение повторяется через минуту, до трёх попыток за сессию; затем стратегия сохраняет позиции до следующей сессии.</p>` : `<p class="ai-rationale">${esc(d.rationale || "Без обоснования")}</p>`}
    ${outcomes ? `<p class="ai-outcome">Итог: ${esc(outcomes)}</p>` : ""}
    ${sources ? `<h4>Документы, которые видела модель</h4><ul class="ai-sources">${sources}</ul>` : ""}
    <p class="detail-note">Инструменты: ${toolList || "не вызывались"}${d.duration_ms ? ` · ${fmt(d.duration_ms/1000,1)} с` : ""}${spend}${d.tool_calls?.confidence_percent!=null ? ` · уверенность ${fmt(d.tool_calls.confidence_percent)}%` : ""}${errors.length ? ` · недоступные ленты: ${esc(errors.join("; "))}` : ""}</p></details>`;
}

function renderAi(c) {
  const models=controlsData?.ai_models;
  if(models){
    $("models-path").textContent=models.path;
    $("models-meta").textContent=!models.exists ? `Файл профилей не найден: ${models.path}` : models.error ? `Файл профилей с ошибкой: ${models.error}` : `Профилей: ${models.models.length} · по умолчанию: ${models.default || "не задан"} · токены в панели не показываются`;
    $("models-meta").className=`muted ${!models.exists || models.error ? "warning" : ""}`;
    const checks=new Map();
    for(const command of (controlsData.commands || []).filter(x=>x.kind==="ai_check"))if(!checks.has(command.payload?.profile || ""))checks.set(command.payload?.profile || "",command);
    const spend=aiSpendHtml();
    if($("ai-spend").dataset.html!==spend){$("ai-spend").innerHTML=spend;$("ai-spend").dataset.html=spend;}
    if(!aiBudgetDirty && document.activeElement!==$("ai-budget") && controlsData?.ai_budget_rub!=null)$("ai-budget").value=Number(controlsData.ai_budget_rub);
    $("models-list").innerHTML=(models.models.length ? table(["Профиль","Провайдер / модель","Адрес API","Токен","Цены за миллион токенов","Проверка"],models.models.map(m=>{
      const check=checks.get(m.name);
      const prices=m.prices ? `вход ${modelPrice(m.prices.input,m.prices.currency)}<span class="subtext">выход ${modelPrice(m.prices.output,m.prices.currency)}${m.prices.cache_read!=null ? ` · из кэша ${modelPrice(m.prices.cache_read,m.prices.currency)}` : ""}</span>` : '<span class="muted-text">не заданы — расход не оценивается</span>';
      return `<tr><td><strong>${esc(m.name)}</strong>${m.default ? '<span class="subtext">по умолчанию</span>' : ""}</td><td>${esc(m.provider==="anthropic" ? "Anthropic" : "OpenAI-совместимый")}<span class="subtext mono">${esc(m.model)}</span></td><td class="mono">${esc(m.url)}${m.web_search ? '<span class="subtext">с веб-поиском</span>' : ""}</td><td>${esc(AI_TOKEN(m.token))}</td><td>${prices}</td><td><button type="button" class="button small" data-ai-check="${esc(m.name)}">Проверить</button>${check ? `<span class="subtext ${check.status==="failed" ? "warning" : check.status==="succeeded" ? "good" : ""}">${esc(commandStates[check.status] || check.status)}: ${esc(commandResult(check))}</span>` : ""}</td></tr>`;
    })) : empty("Профилей нет","Заполните файл профилей моделей — инструкция ниже."));
  }
  const ai=isAiCase(c);
  $("ai-panel").hidden=!ai;
  if(!ai)return;
  const decisions=(controlsData?.ai_decisions || []).filter(d=>d.strategy_id===c.strategy_id);
  $("ai-count").textContent=decisions.length;
  const pending=(controlsData?.commands || []).some(x=>x.kind==="ai_preview" && x.payload?.strategy_id===c.strategy_id && ["queued","running"].includes(x.status));
  $("ai-preview").disabled=pending || !(controlsData?.available && Date.now()-controlsAt<20000);
  $("ai-preview").textContent=pending ? "Модель анализирует…" : "Пробный анализ";
  const html=decisions.length ? decisions.slice(0,40).map(aiDecisionHtml).join("") : empty("Решений пока нет","Первое решение — на ближайшем открытии сессии. «Пробный анализ» покажет решение модели сейчас, без заявок.");
  const theses=(controlsData?.ai_theses || []).filter(t=>t.strategy_id===c.strategy_id);
  $("ai-theses-count").textContent=theses.filter(t=>!t.closed_at).length;
  const DIRECTIONS={long:"покупка",short:"продажа",avoid:"избегать"};
  const condition=x=>x.kind==="price_below" ? `цена ≤ ${fmt(x.value,2)}` : x.kind==="price_above" ? `цена ≥ ${fmt(x.value,2)}` : `дата ${x.value}`;
  const thesesHtml=theses.length ? theses.map(t=>`<div class="ai-thesis ${t.closed_at ? "closed" : ""}"><span class="subtext"><strong>${esc(t.ticker)}</strong> · ${esc(DIRECTIONS[t.direction] || t.direction)} · с ${esc(datetime(t.opened_at))} МСК${t.entry_price!=null ? ` по ${fmt(t.entry_price,2)}` : ""}${t.horizon_days ? ` · горизонт ${t.horizon_days} дн.` : ""}${t.inherited ? " · перенесён" : ""}${t.closed_at ? ` · закрыт ${esc(datetime(t.closed_at))}: ${esc(t.close_reason || "")}` : ""}${t.result_percent!=null ? ` · <span class="${t.result_percent>=0 ? "good" : "warning"}">результат ${t.result_percent>0 ? "+" : ""}${fmt(t.result_percent,2)}%</span>` : ""}</span>${esc(t.thesis)}${(t.conditions || []).length ? `<span class="subtext">Условия: ${esc(t.conditions.map(condition).join("; "))}</span>` : ""}</div>`).join("") : empty("Тезисов пока нет","Модель заводит их на свои идеи по бумагам: с обоснованием, горизонтом и условиями выхода.");
  if($("ai-theses").dataset.html!==thesesHtml){$("ai-theses").innerHTML=thesesHtml;$("ai-theses").dataset.html=thesesHtml;}
  const memory=(controlsData?.ai_memory || []).filter(m=>m.strategy_id===c.strategy_id);
  $("ai-memory-count").textContent=memory.length;
  const MEMORY_KINDS={lesson:"вывод разбора",summary:"сводка старых заметок"};
  const memoryHtml=memory.length ? memory.map(m=>`<div class="ai-note${m.inherited_from ? " inherited" : ""} ${esc(m.kind || "note")}"><span class="subtext">${MEMORY_KINDS[m.kind] ? `<strong>${esc(MEMORY_KINDS[m.kind])}</strong> · ` : ""}${esc(datetime(m.created_at))} МСК${(m.tickers || []).length ? " · "+esc(m.tickers.join(", ")) : ""}${m.expires_at ? " · до "+esc(datetime(m.expires_at)) : ""}${m.inherited_from ? " · перенесено из предыдущей версии портфеля" : ""}</span>${esc(m.note)}</div>`).join("") : empty("Заметок пока нет","Модель записывает сюда долгосрочные идеи и условия выхода, чтобы помнить их между решениями.");
  if($("ai-memory").dataset.html!==memoryHtml){$("ai-memory").innerHTML=memoryHtml;$("ai-memory").dataset.html=memoryHtml;}
  // Keep opened details while the page refreshes.
  if($("ai-decisions").dataset.html!==html){
    const open=new Set([...$("ai-decisions").querySelectorAll("details[open]")].map(d=>d.querySelector("summary span")?.textContent));
    $("ai-decisions").innerHTML=html;$("ai-decisions").dataset.html=html;
    for(const details of $("ai-decisions").querySelectorAll("details"))if(open.has(details.querySelector("summary span")?.textContent))details.open=true;
  }
}

$("models-list").addEventListener("click",e=>{const b=e.target.closest("[data-ai-check]");if(b)issue("ai_check",{profile:b.dataset.aiCheck},true);});
$("ai-preview").addEventListener("click",()=>{const c=data && selectedCase();if(c)issue("ai_preview",{strategy_id:c.strategy_id},true);});

let aiBudgetDirty=false;
$("ai-budget").addEventListener("input",()=>{aiBudgetDirty=true;});
$("ai-budget-form").addEventListener("submit",async e=>{
  e.preventDefault();
  const command=await issue("ai_budget",{rub:$("ai-budget").value},true);
  if(command?.status==="succeeded")aiBudgetDirty=false;
});
