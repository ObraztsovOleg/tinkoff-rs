"use strict";

let logData=null, logKey="", logRequest=0, logFetched=0;
const expandedLogs=new Set();
const logLevelNames={ERROR:"Ошибка",WARN:"Внимание",INFO:"Событие",DEBUG:"Отладка",TRACE:"Отладка",RAW:"Вывод"};
// Fields that change on every repeat of the same event; they do not split a group.
const volatileLogFields=new Set(["event_id","pending","completed","attempt","delay","gap_started","silent_seconds"]);
const logTime=value=>date(value,{hour:"2-digit",minute:"2-digit",second:"2-digit"});

function eventSource(id="") {
  return id.startsWith("risk-check:") ? "плановая проверка риска" : id.startsWith("candle:") ? "закрытие свечи" : id.startsWith("manual:") ? "ручная заявка" : id.startsWith("cleanup:") ? "закрытие позиций"
    : /^schedule:SessionOpen:.*:retry:\d+$/.test(id) ? "повтор решения открытия сессии" : id.startsWith("schedule:SessionOpen:") ? "открытие сессии" : id.startsWith("schedule:SessionClose:") ? "закрытие сессии" : "";
}
// A risk check outside trading hours is routine, not a missed trading signal.
const isRiskCheck=f=>String(f.event_id || "").startsWith("risk-check:");

// Frequent risk-manager reasons in plain Russian; unknown reasons stay as they are.
function riskReason(reason="") {
  if(/quote is stale or future-dated/.test(reason))return "котировка устарела — торги сейчас не идут, решение ждёт свежих цен";
  if(/close-only|drawdown|daily loss/.test(reason))return "сработал лимит риска (дневной стоп или просадка): разрешено только закрытие";
  return reason || "причина не указана";
}

function describeLog(e) {
  const f=e.fields || {}, m=e.message || "", source=eventSource(f.event_id), via=source ? ` · ${source}` : "";
  const rules=[
    [m==="account control service ready; automatic signals disabled",()=>["Служебный процесс: контроль счёта и команд управления. Автоматические сигналы выключены","muted"]],
    [m==="waiting for candle subscription acknowledgements",()=>[`Подписка на свечи: ждём подтверждения потока (${f.subscriptions || "?"} подписок)`,"info"]],
    [m==="candle subscriptions acknowledged",()=>[Number(f.pending)===0 ? "Все подписки на свечи подтверждены" : `Подписки на свечи подтверждаются, осталось ${f.pending}`,"info"]],
    [m==="warmup progress",()=>[`Прогрев истории: ${f.completed || "?"} из ${f.total || "?"} инструментов`,"info"]],
    [m.startsWith("warmup complete"),()=>["Прогрев завершён — стратегия в работе и ждёт рыночных событий","good"]],
    [m.startsWith("startup checks complete"),()=>["Стартовые проверки пройдены","good"]],
    [m==="live decision observed",()=>f.duplicate==="true" ? [`Повтор уже обработанного события${via}`,"muted"] : f.target==="true" ? [Number(f.legs)>0 ? `Сигнал: новая цель портфеля, заявок к отправке: ${f.legs}${via}` : `Сигнал: цель портфеля без новых заявок${via}`,"decision"] : [`Решение: позиции не меняются${via}`,"muted"]],
    [m==="decision uncovered: market orders unavailable" && isRiskCheck(f),()=>["Плановая проверка риска: торги сейчас закрыты, заявки не нужны","muted"]],
    [m==="decision uncovered: market orders unavailable",()=>[`Решение отложено: рыночные заявки сейчас недоступны — торги закрыты, аукцион или инструмент не торгуется${via}`,"warn"]],
    [m==="decision uncovered: risk check rejected execution" && isRiskCheck(f),()=>[`Плановая проверка риска: ${riskReason(f.reason)}`,"muted"]],
    [m.startsWith("session event missed"),()=>[`Событие сессии пропущено: компьютер спал или процесс был занят${via}`,"warn"]],
    [m.startsWith("session-open decision deferred"),()=>[`Решение открытия сессии отложено: повтор каждую минуту${f.until ? " до "+datetime(f.until)+" МСК" : ""}`,"warn"]],
    [m==="retrying deferred session-open decision",()=>["Повтор решения открытия сессии","info"]],
    [m==="deferred session-open decision completed",()=>["Отложенное решение открытия сессии принято","good"]],
    [/^(deferred session-open decision expired|session-open decision missed)/.test(m),()=>["Решение открытия пропущено: следующее — на открытии следующей сессии","warn"]],
    [m==="decision uncovered: market data stream is reconnecting",()=>[`Сигнал пропущен: поток котировок переподключается${via}`,"warn"]],
    [m==="decision uncovered: risk check rejected execution",()=>[`Риск-менеджер отклонил исполнение: ${riskReason(f.reason)}${via}`,"warn"]],
    [m.startsWith("decision uncovered"),()=>[`Сигнал не исполнен: ${m.slice("decision uncovered: ".length)}${via}`,"warn"]],
    [m.startsWith("new trading decision deferred"),()=>["Новый сигнал отложен: предыдущий пакет заявок ещё не завершён","warn"]],
    [m==="risk manager is in close-only mode",()=>["Риск-менеджер: разрешено только закрытие позиций","warn"]],
    [m==="rebalance batch processed",()=>[`Пакет заявок обработан${f.execution ? ": "+f.execution : ""}`,"order"]],
    [m==="recovered durable rebalance batch",()=>[`Восстановлен незавершённый пакет заявок${f.outcome ? ": "+f.outcome : ""}`,"order"]],
    [m==="polled pending rebalance batch",()=>[`Сверка отправленного пакета заявок${f.outcome ? ": "+f.outcome : ""}`,"order"]],
    [m==="maintenance cleanup batch recorded",()=>["Записан пакет служебного закрытия позиций","order"]],
    [m.startsWith("cleanup complete"),()=>["Позиции закрыты, счёт очищен","good"]],
    [/^(cleanup remains requested|protective cleanup waits|protective close remains pending)/.test(m),()=>["Закрытие позиций ждёт открытия торгов или сверки заявок","warn"]],
    [m.startsWith("external position requires manual removal"),()=>["На счёте посторонняя позиция: её нужно закрыть вручную","warn"]],
    [/^market data (connection failed|stream reconnecting|stream is silent|stream is reconnecting)/.test(m) || m.startsWith("runtime session interrupted"),()=>["Связь с потоком котировок прервана, переподключаемся без остановки стратегии","warn"]],
    [m.startsWith("market data reconnected") || m==="market subscriptions confirmed after reconnect" || m.startsWith("network restored"),()=>["Связь восстановлена, решения снова принимаются","good"]],
    [m.startsWith("network or T-Invest API unavailable"),()=>["Сеть или API T-Invest недоступны: решение пропущено, процесс продолжает работу","warn"]],
    [m.startsWith("subscription acknowledgement timeout"),()=>["Нет подтверждения подписки на свечи, повторяем","warn"]],
    [m==="retrying idempotent T-Invest request",()=>[`Повтор запроса к T-Invest${f.attempt ? ", попытка "+f.attempt : ""}`,"warn"]],
    [m.startsWith("server stream connection failed"),()=>[`Поток ${f.stream || "брокера"}: ошибка соединения, переподключаемся`,"warn"]],
    [m==="account command did not complete",()=>[`Команда управления не выполнена${f.kind ? " ("+f.kind+")" : ""}: ${f.error || "без описания"}`,"error"]],
    [m==="command failed",()=>[`Процесс завершился с ошибкой: ${f.error || "без описания"}`,"error"]],
    [m.startsWith("shutdown requested"),()=>["Процесс получил команду остановки","muted"]],
    [m==="database migrations are current",()=>["База данных: миграции актуальны","muted"]],
    [m==="reference data loaded",()=>[`Загружены справочные данные брокера: мультипликаторов ${f.fundamentals || 0}, дивидендов ${f.dividends || 0}`,"info"]],
    [m.startsWith("reference data unavailable"),()=>[`Справочные данные брокера недоступны — стратегия использует прошлый снимок${f.error ? ": "+f.error : ""}`,"warn"]],
  ];
  for(const [hit,make] of rules)if(hit){const [text,tone]=make();return {text,tone};}
  return {text:m || "—",tone:e.level==="ERROR" ? "error" : e.level==="WARN" ? "warn" : "info"};
}

const fieldNames={event_id:"Событие",duplicate:"Повтор",target:"Новая цель портфеля",legs:"Заявок к отправке",strategy_id:"Стратегия (ID)",subscriptions:"Подписок",pending:"Осталось подтвердить",completed:"Загружено",total:"Всего",components:"Стратегий в портфеле",kinds:"Виды стратегий",error:"Ошибка",reason:"Причина",batch_id:"Пакет заявок",execution:"Итог исполнения",outcome:"Итог",attempt:"Попытка",delay:"Пауза",stream:"Поток",kind:"Команда",id:"Команда (ID)",instrument_id:"Инструмент",account_id:"Счёт",code:"Код ответа",silent_seconds:"Без данных, с",gap_started:"Разрыв с"};
const candleNames={"1m":"1 минута","5m":"5 минут","30m":"30 минут","1h":"1 час",day:"1 день","1d":"1 день",month:"1 месяц"};
const tickerOf=uid=>(typeof allAssets==="function" ? allAssets() : []).find(a=>a.uid===uid)?.ticker || uid;

// What the strategy did at this event, in more detail than the one-line summary.
function logMeaning(e) {
  const f=e.fields || {}, m=e.message || "";
  if(m==="account control service ready; automatic signals disabled")return "Наблюдатель запустил процесс счёта без торговой стратегии, чтобы выполнить команду (включение, бюджет, пополнение, ручная заявка) или проверить риск. Сигналы стратегии в этом режиме не считаются, поэтому заявок от стратегии не будет. Торговый процесс запускается в окне расписания или в режиме «Постоянно, пока включена».";
  if(m==="waiting for candle subscription acknowledgements")return "Процесс подписался на поток свечей по всем инструментам портфеля и ждёт подтверждения от T-Invest. Пока подписки не подтверждены, решения не принимаются.";
  if(m==="candle subscriptions acknowledged")return "Поток котировок подтвердил очередную подписку на свечи. «Осталось подтвердить» — сколько подписок ещё ждут ответа; при нуле поток готов.";
  if(m==="warmup progress")return "Стратегия загружает историю свечей (прогрев), чтобы индикаторы были рассчитаны до первой сделки. «Загружено / всего» — сколько рядов истории уже получено.";
  if(m.startsWith("warmup complete"))return "История загружена, стартовые проверки пройдены. С этого момента каждое закрытие свечи и плановая проверка риска — повод пересчитать целевой портфель и, если он изменился, выставить заявки.";
  if(m==="live decision observed"){
    if(f.duplicate==="true")return "Это событие уже было обработано раньше (например, до перезапуска процесса). Повторно оно не исполняется, чтобы не задвоить заявки.";
    if(f.target!=="true")return "Стратегия обработала событие и решила не менять позиции: сигнал не изменил целевой портфель. Это обычный результат — большинство проверок заканчивается так. Плановая проверка риска выполняется примерно раз в 90 секунд: риск-менеджер сверяет стоимость счёта с дневным стопом и максимальной просадкой.";
    return Number(f.legs)>0 ? `Стратегия рассчитала новую цель портфеля и подготовила заявок: ${f.legs}, чтобы привести позиции к цели. Заявки видны в блоке «План и заявки» и во вкладке «Заявки»; после исполнения сделки появятся на графике.` : "Стратегия рассчитала новую цель портфеля, но текущие позиции уже ей соответствуют с точностью до лота, поэтому заявок нет.";
  }
  if(m.startsWith("session event missed"))return "Официальное событие расписания (открытие или закрытие сессии) прошло, пока процесс не работал: компьютер спал, был без сети или процесс был занят. Открытие сессии не теряется: решение повторяется каждую минуту до конца этой сессии.";
  if(m.startsWith("session-open decision deferred"))return "Решение открытия сессии не удалось принять сразу: торги ещё не начались, идёт аукцион, нет сети или исполняется прошлый пакет заявок. Процесс повторяет решение каждую минуту до конца сессии.";
  if(m==="retrying deferred session-open decision")return "Очередная попытка принять отложенное решение открытия сессии. Решение использует свежие цены и закрытые свечи, поэтому позднее исполнение безопасно.";
  if(m==="deferred session-open decision completed")return "Отложенное решение открытия сессии принято: стратегия рассчитала цель и при необходимости выставила заявки.";
  if(/^(deferred session-open decision expired|session-open decision missed)/.test(m))return "До конца сессии решение так и не удалось принять. Следующая попытка — на открытии следующей торговой сессии.";
  if(m==="decision uncovered: market orders unavailable" && isRiskCheck(f))return "Раз в 30 секунд процесс проверяет риск счёта. Сейчас торги закрыты, поэтому заявки не нужны и не отправляются. Это не пропущенный сигнал.";
  if(m==="decision uncovered: market orders unavailable")return "Стратегия хотела выставить рыночные заявки, но инструмент сейчас не торгуется: торги закрыты, идёт аукцион или перерыв. Решение пропущено; на следующем событии во время торгов стратегия пересчитает цель заново.";
  if(m==="decision uncovered: market data stream is reconnecting")return "В момент события поток котировок переподключался. Чтобы не торговать по устаревшим ценам, решение пропущено.";
  if(m==="decision uncovered: risk check rejected execution")return /quote is stale/.test(f.reason || "") ? "Перед заявкой риск-менеджер проверяет свежую котировку: цена последней сделки должна быть не старше пяти минут. Вне торговой сессии (вечер, выходные) это условие не выполняется, поэтому решение пропускается без ошибки; на открытии сессии стратегия пересчитает цель по свежим ценам." : `Риск-менеджер запретил исполнение: ${riskReason(f.reason)}. Так защищается бюджет (дневной стоп и максимальная просадка) и ограничения инструмента.`;
  if(m.startsWith("decision uncovered"))return "Сигнал не был исполнен; причина указана в тексте события. Следующее событие пересчитает цель.";
  if(m.startsWith("new trading decision deferred"))return "Новый сигнал пришёл, пока предыдущий пакет заявок ещё исполняется. Он будет учтён после завершения пакета.";
  if(m==="risk manager is in close-only mode")return "Риск-менеджер перевёл счёт в режим «только закрытие»: достигнут дневной стоп или максимальная просадка. Новые позиции не открываются, существующие можно только закрывать.";
  if(/batch/.test(m))return "Пакет заявок — все заявки, нужные для перехода к новой цели портфеля. Здесь записан его итог: отправлен, исполнен полностью или частично, отменён.";
  if(m.startsWith("cleanup complete"))return "Все позиции счёта закрыты, активных заявок нет. Счёт свободен для следующего окна или другой стратегии.";
  if(/^(cleanup remains requested|protective cleanup waits|protective close remains pending)/.test(m))return "Закрытие позиций запрошено, но торги сейчас закрыты или заявки ещё сверяются. Процесс повторит закрытие, когда торги откроются.";
  if(m.startsWith("external position requires manual removal"))return "На счёте есть позиция по инструменту вне состава портфеля. Автоматически она не закрывается: закройте её ручной заявкой или добавьте инструмент в портфель.";
  if(/^market data|^runtime session interrupted|^network|^subscription acknowledgement|^server stream/.test(m))return "Соединение с T-Invest прервалось или не ответило вовремя. Процесс переподключается сам, без остановки стратегии; решения во время разрыва пропускаются.";
  if(m==="retrying idempotent T-Invest request")return "Запрос к T-Invest не прошёл с первого раза и повторяется. Повторяются только безопасные запросы, которые не могут создать вторую заявку.";
  if(m==="account command did not complete")return "Команда из панели управления не выполнена. Причина — в поле «Ошибка»; результат также записан в журнал управления.";
  if(m==="reference data loaded")return "Перед открытием сессии процесс запросил у T-Invest текущие мультипликаторы (P/B, P/E, EV/EBITDA, ROE, ROA, маржа, долг) и/или календарь дивидендов и сохранил их в состоянии стратегии. Сами по себе эти данные заявок не создают: решение принимается на открытии сессии.";
  if(m.startsWith("reference data unavailable"))return "Запрос справочных данных к T-Invest не прошёл. Стратегия продолжает работать на последнем сохранённом снимке; если снимка ещё нет, value, quality и дивидендная стратегия пропускают решение до следующей сессии.";
  if(m==="command failed")return "Процесс завершился с ошибкой. Наблюдатель перезапустит его: первые попытки сразу, затем с растущей паузой до 15 минут. Сетевая ошибка обычно проходит сама; ошибка конфигурации требует изменить состав портфеля.";
  if(m.startsWith("shutdown requested"))return "Наблюдатель остановил процесс: закончилось окно расписания, стратегию выключили или стенд перезапускается. Позиции при остановке не закрываются, если не включено «Закрывать позиции после каждого окна».";
  return "Служебное сообщение процесса. Полный текст — в исходной строке ниже.";
}

function eventDetails(id="") {
  const candle=id.match(/^candle:([0-9a-f-]{36}):([^:]+):(.+)$/i);
  if(candle)return `Закрытие свечи ${tickerOf(candle[1])} (${candleNames[candle[2]] || candle[2]}) от ${datetime(candle[3])} МСК`;
  const session=id.match(/^schedule:(SessionOpen|SessionClose|ClosingAuctionStart):(\d{4}-\d\d-\d\d):[^]*?(?::retry:(\d+))?$/);
  if(session)return `${{SessionOpen:"Открытие",SessionClose:"Закрытие",ClosingAuctionStart:"Аукцион закрытия"}[session[1]]} сессии ${date(session[2]+"T12:00:00Z")}${session[3] ? `, повтор №${session[3]}` : ""}`;
  const risk=id.match(/^risk-check:(\d+)$/);
  if(risk)return `Плановая проверка риска в ${logTime(new Date(Number(risk[1])*1000).toISOString())} МСК`;
  return id.startsWith("manual:") ? "Ручная заявка из панели" : id.startsWith("cleanup:") ? "Закрытие позиций" : id;
}

// Targets and orders recorded by the strategy around this log event.
function relatedData(c,e) {
  const f=e.fields || {}, at=Date.parse(e.time || ""), parts=[];
  if(f.batch_id){
    const legs=data.orders.filter(o=>o.batch_id===f.batch_id);
    if(legs.length)parts.push("Заявки пакета: "+legs.map(o=>`${o.instrument} ${/buy/i.test(o.side) ? "покупка" : "продажа"} ${fmt(o.lots)} лот. — ${status(o.status)}`).join("; "));
  }
  if(e.message==="live decision observed" && f.target==="true" && Number.isFinite(at)){
    const target=(data.targets || []).filter(t=>t.strategy===c.kind && Math.abs(Date.parse(t.generated_at)-at)<300000).sort((a,b)=>Math.abs(Date.parse(a.generated_at)-at)-Math.abs(Date.parse(b.generated_at)-at))[0];
    if(target)parts.push("Цель портфеля: "+target.weights.map(w=>`${tickerOf(w.instrument_id)} ${fmt(w.weight*100,2)}%`).join(", "));
    const orders=data.orders.filter(o=>o.strategy===c.kind && Math.abs(Date.parse(o.created_at)-at)<120000);
    if(orders.length)parts.push("Заявки: "+orders.map(o=>`${o.instrument} ${/buy/i.test(o.side) ? "покупка" : "продажа"} ${fmt(o.lots)} лот. — ${status(o.status)}`).join("; "));
  }
  return parts;
}

function logDetails(c,group) {
  const e=group.last, f=e.fields || {};
  const fields=Object.entries(f).map(([key,value])=>{
    const shown=key==="event_id" ? eventDetails(value) : key==="instrument_id" ? tickerOf(value) : value==="true" ? "да" : value==="false" ? "нет" : value;
    return `<tr><th scope="row">${esc(fieldNames[key] || key)}</th><td>${esc(shown)}${key==="event_id" && shown!==value ? `<span class="subtext mono">${esc(value)}</span>` : ""}</td></tr>`;
  }).join("");
  const related=relatedData(c,e);
  return `<div class="log-details"><p><strong>${esc(describeLog(e).text)}</strong></p><p>${esc(logMeaning(e))}</p>${group.count>1 ? `<p class="detail-note">Повторилось ${fmt(group.count)} раз: с ${esc(logTime(group.first.time))} по ${esc(logTime(e.time))} МСК.</p>` : ""}${related.map(text=>`<p class="log-related">${esc(text)}</p>`).join("")}${fields ? `<table class="log-fields"><tbody>${fields}</tbody></table>` : ""}<code>${esc(rawLogLine(e))}</code></div>`;
}

function rawLogLine(e) {
  if(e.level==="RAW")return e.message;
  return `${e.time} ${e.level} ${e.target}: ${e.message}${Object.entries(e.fields || {}).map(([k,v])=>` ${k}=${v}`).join("")}`;
}

function groupLogs(entries) {
  const groups=[];
  for(const e of entries){
    const signature=[e.level,e.target,e.message,...Object.entries(e.fields || {}).filter(([k])=>!volatileLogFields.has(k)).map(([k,v])=>k+"="+v)].join("|");
    const last=groups.at(-1);
    if(last && last.signature===signature){last.count++;last.last=e;continue;}
    groups.push({signature,count:1,first:e,last:e});
  }
  return groups;
}

function logMatches(group,filter) {
  const e=group.last, tone=describeLog(e).tone;
  // Routine risk checks outside trading hours are not warnings, whatever their log level.
  if(filter==="important")return (["WARN","ERROR","RAW"].includes(e.level) && tone!=="muted") || ["warn","error"].includes(tone);
  if(filter==="decisions")return ["decision","order"].includes(tone) || /decision|batch|cleanup/.test(e.message || "");
  return true;
}

// Entries of the current process start, so an old "warmup complete" does not count.
function currentRunEntries(c) {
  const entries=logData?.strategy===c.kind && logData.source==="main" ? logData.entries : [];
  const started=Date.parse(c.last_started || "");
  return Number.isFinite(started) ? entries.filter(e=>!e.time || Date.parse(e.time)>=started-1000) : entries;
}

function strategyNow(c) {
  const control=typeof controlFor==="function" ? controlFor(c) : null;
  const schedule=c.schedule || {}, always=(control?.run_mode || schedule.run_mode)==="always";
  const enabled=control?.enabled!==false, current=Date.now();
  const next=(schedule.windows || []).find(w=>Date.parse(w.end)>current);
  const run=currentRunEntries(c), live=run.some(e=>(e.message || "").startsWith("warmup complete"));
  const limit=typeof controlsData!=="undefined" && controlsData?.max_runtimes || 4;
  const plan=next ? `Ближайшее окно: ${dayRange(next.days)} — прогрев с ${datetime(next.begin)}, торговля ${datetime(next.open)} – ${datetime(next.end)} МСК.` : "";
  const hint="Дни задаются в блоке «Когда торговать»; «Постоянно, пока включена» запускает торговлю сразу.";
  if(!enabled)return ["Стратегия выключена",(c.running ? "Новые автоматические заявки запрещены. Процесс продолжает контроль риска и выполняет команды управления." : "Новые заявки запрещены, процесс остановлен. Нажмите «Включить», чтобы разрешить торговлю.")+(plan ? " Расписание сохранено, но без включения торговли не будет." : ""),"warn"];
  if(c.running && c.service_only)return ["Работает служебный процесс","Процесс выполняет команду управления и следит за счётом; сигналов и заявок нет. "+(always || schedule.active ? "Наблюдатель заменит его торговым процессом в ближайшем цикле." : plan || hint),"warn"];
  if(c.running && !live)return ["Запуск и прогрев","Процесс подписывается на свечи и загружает историю. После прогрева стратегия начнёт принимать решения по рыночным событиям.","info"];
  if(c.running){
    const open=nearestSession(), soon=open && Date.parse(open.startTime)>current ? `Ближайшее открытие: ${weekday(open.startTime)}, ${time(open.startTime)} МСК. ` : open ? "Сессия идёт. " : "";
    return ["Стратегия торгует",`Решение принимается на открытии торговой сессии: стратегия считает цель по закрытым свечам и выставляет заявки, если цель изменилась. ${soon}Месячные стратегии меняют позиции раз в месяц, первый раз — на первом открытии после запуска. Между открытиями раз в 30 секунд идёт проверка риска; вне торгов она заявок не создаёт.`+(always ? " Режим: постоянно, пока включена." : ""),"good"];
  }
  if(c.state==="restart backoff")return ["Пауза перед перезапуском",`Процесс аварийно завершился. Повторный запуск: ${datetime(c.retry_at)} МСК. Причина — в журнале ниже.`,"error"];
  if(c.state==="waiting for runtime slot")return ["Ждёт свободного места",`Одновременно работают не больше ${limit} процессов (настройка «Процессов одновременно»). Стратегия запустится, когда место освободится.`,"warn"];
  if(/waiting for (predecessor|account owner)/.test(c.state || ""))return ["Ждёт освобождения счёта","Счёт занят другой стратегией. Запуск — после закрытия её позиций.","warn"];
  if(/failed/.test(c.state || ""))return ["Запуск не удался","Подробности — в журнале ниже.","error"];
  if(c.state==="cleanup_pending")return ["Закрытие позиций после окна","Позиции закрываются, как только торги позволят. "+plan,"warn"];
  if(always || schedule.active)return ["Запускается","Наблюдатель стартует процесс в ближайшие секунды.","info"];
  if(next)return [c.state==="cleaned" ? "Позиции закрыты, ждёт следующее окно" : "Ожидает своё окно",plan,"muted"];
  return [c.state==="cleaned" ? "Позиции закрыты" : "Дни торговли не выбраны","Окон в расписании нет, поэтому стратегия не торгует. "+hint,"muted"];
}

function renderStrategyNow() {
  const c=data && selectedCase();if(!c || !$("strategy-now"))return;
  const [title,detail,tone]=strategyNow(c);
  const entries=logData?.strategy===c.kind && logData.source==="main" ? logData.entries : [];
  const last=entries.at(-1);
  $("strategy-now").className=`strategy-now tone-${tone}`;
  $("strategy-now").innerHTML=`<span class="eyebrow">СЕЙЧАС</span><strong>${esc(title)}</strong><p>${esc(detail)}</p><span class="now-last">${last ? `Последнее событие ${esc(logTime(last.time))} МСК: ${esc(describeLog(last).text)}` : "В журнале основного процесса пока нет событий."}</span>`;
}

function renderLogs() {
  const c=data && selectedCase();if(!c || !$("log-entries"))return;
  renderStrategyNow();
  const source=$("log-source").value;
  for(const option of $("log-source").options)option.textContent=option.textContent.replace(/ · нет файла$/,"")+(logData?.strategy===c.kind && !logData.sources.includes(option.value) ? " · нет файла" : "");
  if(!logData || logData.strategy!==c.kind || logData.source!==source){$("log-count").textContent="…";$("log-entries").innerHTML=empty("Загружаем журнал…","");return;}
  if(!logData.exists){$("log-count").textContent="0";$("log-meta").textContent="Журнал появится после первого запуска процесса.";$("log-entries").innerHTML=empty("Процесс ещё не запускался",source==="main" ? "Записи появятся, когда наблюдатель или команда «Включить» запустят стратегию." : "Этот этап для стратегии ещё не выполнялся.");return;}
  const filter=$("log-level").value, raw=$("log-raw").checked;
  const groups=groupLogs(logData.entries).filter(g=>logMatches(g,filter)).reverse();
  $("log-count").textContent=logData.entries.length;
  $("log-meta").textContent=`Последняя запись ${ageText(logData.modified)} · последние ${fmt(logData.entries.length)} строк${logData.truncated ? ", более ранние скрыты" : ""} · новые сверху, повторы объединены`;
  $("log-entries").innerHTML=groups.length ? groups.map(g=>{
    const e=g.last, {text,tone}=describeLog(e), key=`${g.first.time}|${g.signature}`, open=expandedLogs.has(key);
    return `<div class="log-row tone-${esc(tone)}${open ? " expanded" : ""}" role="button" tabindex="0" aria-expanded="${open}" data-log-key="${esc(key)}" title="Нажмите, чтобы увидеть подробности"><time datetime="${esc(e.time || "")}">${esc(e.time ? logTime(e.time) : "—")}</time><span class="log-level level-${esc(e.level.toLowerCase())}">${esc(logLevelNames[e.level] || e.level)}</span><div class="log-text">${esc(text)}${g.count>1 ? `<span class="log-repeat">×${fmt(g.count)} · с ${esc(logTime(g.first.time))}</span>` : ""}${raw && !open ? `<code>${esc(rawLogLine(e))}</code>` : ""}</div>${open ? logDetails(c,g) : ""}</div>`;
  }).join("") : empty("Нет событий по фильтру","Выберите «Все события», чтобы увидеть весь журнал.");
}

async function loadLogs(force=false) {
  const c=data && selectedCase();if(!c)return;
  const source=$("log-source").value, key=`${c.kind}:${source}`;
  if(!force && key===logKey && Date.now()-logFetched<4000){renderLogs();return;}
  if(key!==logKey){logData=null;renderLogs();}
  logKey=key;logFetched=Date.now();
  const request=++logRequest;
  try{
    const response=await fetch(`/api/logs?strategy=${encodeURIComponent(c.kind)}&source=${encodeURIComponent(source)}&limit=400`,{cache:"no-store",signal:AbortSignal.timeout(8000)});
    if(!response.ok)throw Error("logs unavailable");
    const result=await response.json();if(request!==logRequest)return;
    logData=result;renderLogs();
  }catch(_){
    if(request!==logRequest)return;
    $("log-meta").textContent="Журнал временно недоступен. Повторим автоматически.";
  }
}

$("log-source").addEventListener("change",()=>loadLogs(true));
$("log-level").addEventListener("change",renderLogs);
$("log-raw").addEventListener("change",renderLogs);
$("log-refresh").addEventListener("click",()=>loadLogs(true));
function toggleLog(row) {
  const key=row.dataset.logKey;
  expandedLogs.has(key) ? expandedLogs.delete(key) : expandedLogs.add(key);
  renderLogs();
  $("log-entries").querySelector(`[data-log-key="${CSS.escape(key)}"]`)?.focus();
}
$("log-entries").addEventListener("click",e=>{const row=e.target.closest("[data-log-key]");if(row && !e.target.closest(".log-details"))toggleLog(row);});
$("log-entries").addEventListener("keydown",e=>{const row=e.target.closest("[data-log-key]");if(row && e.target===row && (e.key==="Enter" || e.key===" ")){e.preventDefault();toggleLog(row);}});
setInterval(()=>{if(!document.hidden && data && selectedCase())loadLogs();},5000);
