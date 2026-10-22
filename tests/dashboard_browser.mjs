import {spawn} from 'node:child_process';
import {mkdtemp,readFile,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
const profile=await mkdtemp(join(tmpdir(),'sandbox-dashboard-chrome-'));
const chrome=spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',['--headless=new','--disable-gpu','--no-first-run','--no-default-browser-check',`--user-data-dir=${profile}`,'--remote-debugging-port=0','about:blank'],{stdio:'ignore'});
let socket;
const pause=ms=>new Promise(r=>setTimeout(r,ms));
try {
 let port;
 for(let i=0;i<100;i++){try{port=(await readFile(join(profile,'DevToolsActivePort'),'utf8')).split('\n')[0];break;}catch{await pause(100);}}
 if(!port)throw Error('Chrome did not start');
 const pages=await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
 socket=new WebSocket(pages.find(p=>p.type==='page').webSocketDebuggerUrl);
 await new Promise((r,j)=>{socket.addEventListener('open',r,{once:true});socket.addEventListener('error',j,{once:true});});
 let id=0;const pending=new Map(), errors=[];
 socket.addEventListener('message',e=>{const m=JSON.parse(e.data);if(m.id){const p=pending.get(m.id);pending.delete(m.id);m.error?p.reject(Error(JSON.stringify(m.error))):p.resolve(m.result);}if(m.method==='Runtime.exceptionThrown')errors.push(m.params.exceptionDetails.text);if(m.method==='Log.entryAdded' && m.params.entry.level==='error')errors.push(m.params.entry.text);});
 const call=(method,params={})=>new Promise((resolve,reject)=>{const n=++id;pending.set(n,{resolve,reject});socket.send(JSON.stringify({id:n,method,params}));});
 const evaluate=async expression=>{const r=await call('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true});if(r.exceptionDetails)throw Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);return r.result.value;};
 const assert=(value,message)=>{if(!value)throw Error(message);};

 await call('Page.enable');await call('Runtime.enable');await call('Log.enable');
 await call('Emulation.setDeviceMetricsOverride',{width:1600,height:1100,deviceScaleFactor:1,mobile:false});
 await call('Page.navigate',{url:'http://127.0.0.1:18766'});
 const waitFor=async(expr)=>{for(let i=0;i<100;i++){if(await evaluate(expr))return;await pause(100);}throw Error('Timeout: '+expr);};
 await waitFor('typeof controlsData !== "undefined" && controlsData?.available && data?.cases?.length===4');
 // Only a hint about the nearest MOEX session, no fixed campaign window.
 assert(await evaluate('document.getElementById("session-hint").textContent.startsWith("Ближайшая торговая сессия:") && !document.getElementById("sessions")'),'Nearest session hint missing');
 // A sleep of the computer during a trading session is reported as a missed session.
 assert(await evaluate('!document.getElementById("notice").hidden && document.getElementById("notice").textContent.includes("Компьютер спал") && document.getElementById("notice").textContent.includes("пропущена")'),'Missed session after sleep not reported');
 // Storage: low space on the Mac disk is a warning, the panel shows both disks, upkeep and backups.
 await waitFor('document.getElementById("notice").textContent.includes("Мало свободного места: 2 ГБ (Диск Mac)")');
 assert(await evaluate('(()=>{const t=document.getElementById("storage-content").textContent;return t.includes("Диск Mac: свободно 2 ГБ") && t.includes("Том стенда (VM)") && t.includes("База данных: 23 МБ") && /5\\s600 проверок риска/.test(t) && t.includes("Последняя копия сделана")})()'),'Storage panel wrong: '+await evaluate('document.getElementById("storage-content").textContent'));
 // Sections: the overview shows only its blocks; every topic has its own page.
 const visible=id=>evaluate(`(()=>{const e=document.getElementById(${JSON.stringify(id)});return !!e && e.offsetParent!==null})()`);
 assert(await visible('control-overview') && !(await visible('activity')) && !(await visible('live')) && !(await visible('strategies')),'Overview shows other sections');
 for(const [view,id] of [['activity','activity'],['accounts','accounts'],['live','live'],['ranking','ranking'],['instruments','instrument-catalog'],['journal','journal'],['strategies','strategies']]){
   await evaluate(`document.querySelector('[data-nav="${view}"]').click()`);
   await waitFor(`location.hash==="#${view}" && document.getElementById(${JSON.stringify(id)}).offsetParent!==null && document.getElementById("control-overview").offsetParent===null && document.querySelector('[data-nav="${view}"]').classList.contains("selected")`);
 }
 assert(await evaluate('document.getElementById("strategy-detail").offsetParent===null && document.querySelectorAll("#strategy-rows tr").length===3'),'Strategy list wrong');
 await evaluate('document.querySelector(\'#strategy-rows [data-strategy="pairs-trading"]\').click()');
 await waitFor('location.hash==="#strategy/pairs-trading" && document.getElementById("strategy-detail").offsetParent!==null && document.getElementById("strategies").offsetParent===null && document.getElementById("view-title").textContent==="pairs-trading"');
 await waitFor('marketSeries?.candles?.length===60');
 assert(await evaluate('document.querySelectorAll("#asset-interval [data-interval]").length===6 && document.querySelector(\'#asset-interval [aria-checked="true"]\').dataset.interval==="day"'),'Interval buttons missing');
 // Strategy journal: readable text, newest first, grouped repeats, redaction and filters.
 await waitFor('logData?.strategy==="pairs-trading" && document.querySelectorAll("#log-entries .log-row").length>0');
 assert(await evaluate('document.getElementById("strategy-now").textContent.includes("Стратегия торгует")'),'Current activity missing');
 assert(await evaluate('document.querySelector("#log-entries .log-row").textContent.includes("Отложенное решение открытия сессии принято")'),'Log not newest first');
 assert(await evaluate('[...document.querySelectorAll("#log-entries .log-row")].some(r=>r.textContent.includes("Плановая проверка риска: торги сейчас закрыты") && r.textContent.includes("×3") && /tone-muted/.test(r.className))'),'Routine risk checks not grouped as neutral');
 assert(await evaluate('["Событие сессии пропущено: компьютер спал","Решение открытия сессии отложено","Повтор решения открытия сессии"].every(t=>document.getElementById("log-entries").textContent.includes(t))'),'Missed open cycle not translated');
 assert(await evaluate('document.getElementById("log-entries").textContent.includes("Прогрев завершён")'),'Warmup event not translated');
 await evaluate('document.getElementById("log-raw").checked=true;document.getElementById("log-raw").dispatchEvent(new Event("change"))');
 assert(await evaluate('document.getElementById("log-entries").textContent.includes("t.***") && !document.getElementById("log-entries").textContent.includes("FIXTURExNOT")'),'Token-like value not redacted');
 await evaluate('document.getElementById("log-level").value="important";document.getElementById("log-level").dispatchEvent(new Event("change"))');
 assert(await evaluate('document.querySelectorAll("#log-entries .log-row").length===3 && [...document.querySelectorAll("#log-entries .log-row")].every(r=>/tone-(warn|error)/.test(r.className))'),'Important filter wrong');
 await evaluate('document.getElementById("log-level").value="decisions";document.getElementById("log-level").dispatchEvent(new Event("change"))');
 assert(await evaluate('[...document.querySelectorAll("#log-entries .log-row")].some(r=>r.textContent.includes("Сигнал: новая цель портфеля"))'),'Decision filter wrong');
 await evaluate('for(const [id,value] of [["log-level","all"]]){document.getElementById(id).value=value;document.getElementById(id).dispatchEvent(new Event("change"));}document.getElementById("log-raw").checked=false;document.getElementById("log-raw").dispatchEvent(new Event("change"))');
 // A click opens the detailed explanation of what the strategy did; Enter closes it.
 await evaluate('[...document.querySelectorAll("#log-entries .log-row")].find(r=>r.textContent.includes("Плановая проверка риска")).click()');
 await waitFor('document.querySelector("#log-entries .log-row.expanded .log-details")!==null');
 assert(await evaluate('(()=>{const t=document.querySelector(".log-details").textContent;return t.includes("проверяет риск счёта") && t.includes("Плановая проверка риска") && t.includes("Повторилось 3 раз") && t.includes("decision uncovered")})()'),'Log details incomplete');
 await evaluate('document.querySelector("#log-entries .log-row.expanded").dispatchEvent(new KeyboardEvent("keydown",{key:"Enter",bubbles:true}))');
 await waitFor('document.querySelector("#log-entries .log-row.expanded")===null');
 await evaluate('[...document.querySelectorAll("#log-entries .log-row")].find(r=>r.textContent.includes("новая цель")).click()');
 await waitFor('document.querySelector(".log-details")?.textContent.includes("Закрытие свечи")');
 // Outside the page: expected 404s would otherwise count as browser console errors.
 const logStatus=async query=>(await fetch('http://127.0.0.1:18766/api/logs?'+query)).status;
 assert(await logStatus('strategy=..%2Fcampaign')===404 && await logStatus('strategy=pairs-trading&source=..%2Fx')===404 && await logStatus('strategy=pairs-trading&source=cleanup')===200,'Log endpoint accepts unknown paths');
 assert(await evaluate('(()=>{const c=selectedCase(),saved={...c};Object.assign(c,{service_only:true,launch_at:new Date(Date.now()+86400000).toISOString()});const text=strategyNow(c).join(" ");Object.assign(c,saved);return text.includes("служебный процесс") && text.includes("Ближайшее окно")})()'),'Service-only process not explained');
 // Operator schedule: own days, a day taken by another portfolio on the account, always mode.
 await waitFor('document.querySelectorAll("#schedule-days .day-chip").length===6');
 assert(await evaluate('document.querySelector(`#schedule-days [data-day="${controlsData.cases[1].trading_days[0]}"]`).disabled'),'Busy account day is selectable');
 assert(await evaluate('document.querySelector(`#schedule-days [data-day="${controlsData.cases[0].trading_days[0]}"]`).getAttribute("aria-pressed")==="true"'),'Saved day not selected');
 await evaluate('document.querySelectorAll("#schedule-days .day-chip:not(:disabled)")[2].click()');
 assert(await evaluate('document.getElementById("schedule-preview").textContent.includes("прогрев с")'),'Schedule preview missing');
 await evaluate('document.getElementById("schedule-form").requestSubmit()');
 await waitFor('controlsData.commands.some(c=>c.kind==="schedule") && controlsData.cases[0].trading_days.length===2');
 await evaluate('document.querySelector(\'[name="run-mode"][value="always"]\').click()');
 assert(await evaluate('document.querySelector("#schedule-days .day-chip").disabled && document.getElementById("schedule-preview").textContent.includes("пока включена")'),'Always mode not reflected');
 await evaluate('document.getElementById("schedule-form").requestSubmit()');
 await waitFor('controlsData.cases[0].run_mode==="always"');
 // Executed trades on the chart: priced markers, unpriced line, out-of-period summary, focus from tables.
 await waitFor('document.querySelectorAll("#asset-chart .trade-buy").length===1 && document.querySelectorAll("#asset-chart .trade-sell").length===1');
 assert(await evaluate('document.querySelectorAll("#asset-chart .trade-unpriced").length===1 && document.querySelectorAll("#asset-chart .trade-label").length===2'),'Unpriced execution or lot labels not drawn');
 assert(await evaluate('document.getElementById("trade-summary").textContent.includes("На графике 3 из 4") && document.getElementById("trade-summary").textContent.includes("1 вне периода")'),'Trade legend wrong: '+await evaluate('document.getElementById("trade-summary").textContent'));
 assert(await evaluate('document.getElementById("executed-count").textContent==="4"'),'Executed trade count wrong');
 await evaluate(`document.querySelector('[data-show-trade="trade:trade-sell"]').click()`);
 await waitFor(`document.querySelector('#asset-chart [data-trade="trade:trade-sell"] .trade-highlight')!==null`);
 await evaluate('chooseStrategy("");document.getElementById("tab-fills").click()');
 await waitFor(`document.querySelector('[data-trade-key="trade:trade-buy"]')!==null`);
 await evaluate(`document.querySelector('[data-trade-key="trade:trade-buy"]').click()`);
 await waitFor(`selectedCase()?.kind==="pairs-trading" && document.querySelector('#asset-chart [data-trade="trade:trade-buy"] .trade-highlight')!==null`);
 await evaluate('document.getElementById("tab-orders").click()');
 // Terminal-style chart: interval buttons, a visible window of readable candles, pan, zoom, crosshair.
 await evaluate(`document.querySelector('#asset-interval [data-interval="5s"]').click()`);
 await waitFor('marketSeries?.interval==="5s" && chartCandles.length===600 && chartGeometry');
 assert(await evaluate('chartView.count<600 && visibleCandles.length===chartView.count && chartView.end===600 && chartView.follow && document.getElementById("chart-range").textContent.includes("из 600")'),'Candles are not windowed: '+await evaluate('JSON.stringify(chartView)'));
 assert(await evaluate('(()=>{const bodies=document.querySelectorAll("#asset-chart rect.candle-up,#asset-chart rect.candle-down");return bodies.length===chartView.count && Number(bodies[0].getAttribute("width"))>=4})()'),'Candles too thin');
 assert(await evaluate('document.querySelectorAll("#asset-chart text.chart-label").length>=8 && document.querySelector("#asset-chart .last-price-box")!==null'),'Price/time axes missing');
 await evaluate(`document.querySelector('[data-chart="back"]').click()`);
 assert(await evaluate(`chartView.end<600 && !chartView.follow && !document.querySelector('[data-chart="latest"]').disabled`),'Back pan failed');
 const panned=await evaluate('chartView.end');
 const pointer=(type,dx,extra='')=>evaluate(`(()=>{const s=document.getElementById("asset-chart"),r=s.getBoundingClientRect();s.dispatchEvent(new PointerEvent("${type}",{clientX:r.left+r.width/2+${dx},clientY:r.top+r.height/3,button:0,pointerId:7,bubbles:true${extra}}))})()`);
 await pointer('pointerdown',0);await pointer('pointermove',200);await pointer('pointerup',200);
 assert(await evaluate(`chartView.end<${panned}`),'Dragging right did not show older candles');
 await pointer('pointermove',-100);
 assert(await evaluate('document.getElementById("chart-crosshair").getAttribute("visibility")==="visible" && /\\d/.test(document.getElementById("cross-price").textContent) && /\\d\\d:\\d\\d:\\d\\d/.test(document.getElementById("cross-time").textContent)'),'Crosshair missing price or time');
 assert(await evaluate('document.getElementById("asset-ohlc").textContent.includes("Откр") && document.getElementById("asset-ohlc").textContent.includes("Объём")'),'OHLC line missing');
 const zoomed=await evaluate('chartView.count');
 await evaluate('(()=>{const s=document.getElementById("asset-chart"),r=s.getBoundingClientRect();s.dispatchEvent(new WheelEvent("wheel",{deltaY:-120,clientX:r.left+r.width/2,clientY:r.top+r.height/3,bubbles:true,cancelable:true}))})()');
 assert(await evaluate(`chartView.count<${zoomed}`),'Wheel zoom failed');
 await evaluate(`document.querySelector('[data-chart="zoom-out"]').click()`);
 assert(await evaluate(`chartView.count>=${zoomed}`),'Zoom out failed');
 await evaluate('document.getElementById("asset-chart").focus();document.getElementById("asset-chart").dispatchEvent(new KeyboardEvent("keydown",{key:"End",bubbles:true}))');
 assert(await evaluate('chartView.end===600 && chartView.follow && document.querySelector(\'[data-chart="latest"]\').disabled'),'End key did not return to the newest candle');
 await pointer('pointerleave',0);
 await evaluate(`document.querySelector('#asset-interval [data-interval="day"]').click()`);
 await waitFor('marketSeries?.interval==="day" && chartCandles.length===60');
 await evaluate('document.querySelector("[data-command=pause]").click()');
 await waitFor('controlsData.commands.some(c=>c.kind==="pause")');
 assert(await evaluate('controlFor(selectedCase()).enabled===false'),'Pause not reflected');
 await evaluate('document.querySelector("[data-command=resume]").click()');
 await waitFor('controlFor(selectedCase()).enabled===true');
 await evaluate('document.getElementById("manual-lots").value="3";document.getElementById("manual-type").value="limit";document.getElementById("manual-type").dispatchEvent(new Event("change"));document.getElementById("manual-price").value="123.45";document.getElementById("manual-form").requestSubmit()');
 await waitFor('controlsData.commands.some(c=>c.kind==="manual_order")');
 assert(await evaluate('controlsData.commands.find(c=>c.kind==="manual_order").payload.lots===3 && controlsData.commands.find(c=>c.kind==="manual_order").payload.price==="123.45"'),'Manual payload corrupted');
 await evaluate('document.getElementById("manual-form").requestSubmit()');
 await waitFor('controlsData.commands.filter(c=>c.kind==="manual_order").length===2');
 // Budget: one target amount; the difference is computed and journaled.
 assert(await evaluate('!document.getElementById("budget-operation")'),'Budget still offers add/subtract');
 await evaluate('document.getElementById("budget-amount").value="900000";document.getElementById("budget-amount").dispatchEvent(new Event("input"))');
 assert(await evaluate('/[-−]100/.test(document.getElementById("budget-delta").textContent)'),'Budget difference preview missing: '+await evaluate('document.getElementById("budget-delta").textContent'));
 await evaluate('document.getElementById("budget-form").requestSubmit()');
 await waitFor('controlsData.commands.some(c=>c.kind==="budget") && document.getElementById("budget-history").textContent.includes("900")');
 assert(await evaluate('controlsData.commands.find(c=>c.kind==="budget").payload.operation==="set" && document.getElementById("budget-history").textContent.includes("→") && /[-−]100/.test(document.getElementById("budget-history").textContent)'),'Budget journal incomplete: '+await evaluate('document.getElementById("budget-history").textContent'));
 await evaluate('document.getElementById("payin-amount").value="1000";document.getElementById("payin-form").requestSubmit()');
 await waitFor('controlsData.commands.some(c=>c.kind==="pay_in")');
 await evaluate('document.querySelector("[data-plan-action=approve_batch]").click()');
 await waitFor('controlsData.commands.some(c=>c.kind==="approve_batch")');
 await evaluate('document.querySelector("[data-plan-action=cancel_batch]").click()');
 await waitFor('controlsData.commands.some(c=>c.kind==="cancel_batch")');
 await evaluate('document.getElementById("instrument-query").value="TEST";document.getElementById("search-form").requestSubmit()');
 await waitFor('document.querySelector("[data-add-asset]")!==null');
 // Any T-Invest type is found; a continuous futures series is offered; what the stand cannot trade says why.
 assert(await evaluate('(()=>{const t=document.getElementById("instrument-results").textContent;return t.includes("Фьючерс") && t.includes("непрерывный") && t.includes("пункт цены") && t.includes("сейчас BRX6") && t.includes("Не торгуется: этот тип") && document.querySelectorAll("[data-add-asset]").length===2})()'),'Catalog search by type wrong: '+await evaluate('document.getElementById("instrument-results").textContent.slice(0,400)'));
 await evaluate('document.querySelector("[data-add-asset]").click()');
 await waitFor('controlsData.catalog.length===1');
 await evaluate('(async()=>{document.getElementById("asset-select").value="22222222-2222-4222-8222-222222222222";setChartInterval("5s");await refresh();await loadMarket(true)})()');
 await waitFor('marketSeries?.ticker==="TEST" && marketSeries.interval==="5s"');
 assert(await evaluate('document.getElementById("asset-select").value==="22222222-2222-4222-8222-222222222222"'),'Added chart selection lost on refresh');
 assert(await evaluate('document.querySelectorAll("#ranking-content tbody tr").length===1'),'Ranking missing');
 // Portfolios: one card per current portfolio with its account, blocks, AI model and instruments; old scenarios in the archive.
 await evaluate('location.hash="#portfolios"');
 await waitFor('document.querySelectorAll("#portfolio-cards .portfolio-card").length===3');
 assert(await evaluate('(()=>{const t=document.getElementById("portfolio-cards").textContent;return t.includes("ИИ по новостям") && t.includes("ИИ · claude") && t.includes("claude-opus-5-5") && t.includes("Реальный счёт") && t.includes("Песочница") && !t.includes("old-scenario") && document.getElementById("portfolios-archive").textContent==="Архив (1)"})()'),'Portfolios wrong: '+await evaluate('document.getElementById("portfolio-cards").textContent.slice(0,500)'));
 await evaluate('document.getElementById("portfolios-archive").click()');
 await waitFor('document.getElementById("portfolio-cards").textContent.includes("old-scenario")');
 await evaluate('document.getElementById("portfolios-archive").click()');
 assert(await evaluate('!document.title.includes("Sandbox") && !document.querySelector(".brand").textContent.includes("Sandbox")'),'Sandbox name still on the page');
 // AI models: what each model does now and the rating by decision outcomes.
 await evaluate('location.hash="#models"');
 await waitFor('document.getElementById("ai-board").textContent.includes("Думает с")');
 assert(await evaluate('(()=>{const rows=[...document.querySelectorAll("#ai-board table")[1].querySelectorAll("tbody tr")];return rows.length===2 && rows[0].textContent.includes("claude-opus-5-5") && rows[0].textContent.includes("70% лучше индекса") && rows[0].textContent.startsWith("1")})()'),'AI rating wrong: '+await evaluate('document.getElementById("ai-board").textContent.slice(0,600)'));
 // Commissions: the tariff of each contour, where each rate comes from, the trade band.
 await evaluate('location.hash="#accounts"');
 await waitFor('document.getElementById("costs-table").textContent.includes("«Трейдер»")');
 assert(await evaluate('(()=>{const t=document.getElementById("costs-table").textContent;return t.includes("по фактическим списаниям") && t.includes("по тарифу") && t.includes("Фьючерсы") && document.getElementById("band-percent").value==="0.5"})()'),'Commissions wrong: '+await evaluate('document.getElementById("costs-table").textContent.slice(0,400)'));
 await evaluate('document.getElementById("band-percent").value="1";document.getElementById("band-form").requestSubmit()');
 await waitFor('controlsData.commands.some(c=>c.kind==="trade_band" && c.payload.percent==="1")');
 await evaluate('location.hash="#overview"');
 // Portfolio builder: rule errors are shown before sending, then two disjoint blocks are saved.
 await evaluate('document.getElementById("portfolio-editor").open=true');
 await waitFor('document.querySelector("#portfolio-blocks .portfolio-block")!==null');
 assert(await evaluate('document.getElementById("portfolio-check").textContent.includes("требует от 2") && document.querySelector("#portfolio-form [type=submit]").disabled'),'Pairs requirement not checked');
 const field=(block,name,strategy)=>`#portfolio-blocks [data-block="${block}"]${strategy===undefined ? ':not([data-strategy])' : `[data-strategy="${strategy}"]`}[data-field="${name}"]`;
 const setField=async(selector,value)=>evaluate(`(()=>{const e=document.querySelector(${JSON.stringify(selector)});e.value=${JSON.stringify(value)};e.dispatchEvent(new Event("input",{bubbles:true}))})()`);
 await setField(field(0,'kind',0),'moving-average-trend');
 await waitFor('!document.querySelector("#portfolio-form [type=submit]").disabled');
 await evaluate('document.getElementById("portfolio-add-block").click()');
 await waitFor('document.querySelectorAll("#portfolio-blocks .portfolio-block").length===2');
 assert(await evaluate('document.querySelector(`#portfolio-blocks [data-block="1"][data-uid="11111111-1111-4111-8111-111111111111"]`).disabled'),'Instrument of block 1 offered to block 2');
 await evaluate('document.querySelector(`#portfolio-blocks [data-block="1"][data-uid="22222222-2222-4222-8222-222222222222"]`).click()');
 await waitFor('document.getElementById("portfolio-check").textContent.includes("100%")');
 await setField(field(0,'share'),'60');await setField(field(1,'share'),'40');
 await evaluate('document.querySelector(`#portfolio-blocks [data-action="add-strategy"][data-block="1"]`).click()');
 await waitFor('document.querySelectorAll(`#portfolio-blocks [data-block="1"][data-field="kind"]`).length===2');
 await setField(field(1,'kind',1),'market-intraday-momentum');
 await waitFor('document.getElementById("portfolio-check").textContent.startsWith("Готово")');
 await evaluate('document.getElementById("portfolio-title").value="Два блока";document.getElementById("portfolio-title").dispatchEvent(new Event("input"));document.getElementById("portfolio-form").requestSubmit()');
 await waitFor('controlsData.commands.some(c=>c.kind==="portfolio")');
 assert(await evaluate('(()=>{const p=controlsData.commands.find(c=>c.kind==="portfolio").payload;return p.title==="Два блока" && p.blocks.length===2 && p.blocks[0].share==="60" && p.blocks[1].instruments[0]==="22222222-2222-4222-8222-222222222222" && p.blocks[1].strategies.map(s=>s.kind).join()==="moving-average-trend,market-intraday-momentum"})()'),'Portfolio payload wrong');
 // Runtime limit replaces the fixed four-process cap.
 await evaluate('document.getElementById("runtime-limit").value="6";document.getElementById("runtime-limit").dispatchEvent(new Event("input"));document.getElementById("limit-form").requestSubmit()');
 await waitFor('controlsData.max_runtimes===6 && document.getElementById("control-connection").textContent.includes("из 6")');
 // AI models: profiles without tokens, a connection check, decisions and a dry-run analysis.
 await evaluate('location.hash="#models"');
 await waitFor('document.getElementById("models").offsetParent!==null && document.querySelectorAll("#models-list tbody tr").length===2');
 // Spend: today against the daily limit, the week by purpose, profiles without prices flagged.
 assert(await evaluate('(()=>{const t=document.querySelector("#ai-spend .ai-spend").textContent;return t.includes("Сегодня: 108 ₽ из дневного лимита 500 ₽") && t.includes("За 7 дней: 315 ₽") && t.includes("разметка документов 4,14 ₽") && t.includes("82,89 ₽ за $") && t.includes("не оценены") && t.includes("раздел [budget]")})()'),'AI spend wrong: '+await evaluate('document.querySelector("#ai-spend")?.textContent'));
 assert(await evaluate('document.getElementById("ai-budget").value==="500" && document.getElementById("models-list").textContent.includes("4 $ ≈ 332 ₽")'),'AI budget field or ruble prices wrong: '+await evaluate('document.getElementById("models-list").textContent.slice(0,400)'));
 await evaluate('document.getElementById("ai-budget").value="800";document.getElementById("ai-budget-form").requestSubmit()');
 await waitFor('controlsData.commands.some(c=>c.kind==="ai_budget" && c.payload.rub==="800")');
 assert(await evaluate('(()=>{const t=document.getElementById("models-list").textContent.replace(/\\s/g," ");return t.includes("вход 4 $ ≈ 332 ₽") && t.includes("выход 20 $ ≈ 1 658 ₽") && t.includes("не заданы — расход не оценивается")})()'),'Model prices wrong');
 assert(await evaluate('(()=>{const t=document.getElementById("models-list").textContent;return t.includes("задан в файле") && t.includes("из переменной LOCAL_KEY") && t.includes("claude-opus-5-5") && document.getElementById("models-path").textContent==="/run/ai/models.toml"})()'),'Model profiles wrong');
 await evaluate('document.querySelector(\'[data-ai-check="local"]\').click()');
 await waitFor('controlsData.commands.some(c=>c.kind==="ai_check" && c.payload.profile==="local") && document.getElementById("models-list").textContent.includes("готов")');
 // Exposures: a model draft, an operator value, editing a cell journals an immediate command.
 await waitFor('document.querySelectorAll("#exposures-table tbody tr").length===2');
 assert(await evaluate('(()=>{const t=document.getElementById("exposures-table").textContent;return t.includes("нефть и газ") && t.includes("+2") && t.includes("черновик готовится") && document.querySelector("#exposures-table .exposure-cell.operator")!==null})()'),'Exposure matrix wrong: '+await evaluate('document.getElementById("exposures-table").textContent.slice(0,400)'));
 await evaluate('document.querySelector(\'#exposures-table [data-exposure="LKOH|oil"]\').click()');
 await waitFor('!document.getElementById("exposure-form").hidden && document.getElementById("exposure-value").value==="2"');
 await evaluate('document.getElementById("exposure-value").value="1.5";document.getElementById("exposure-note").value="половина выручки";document.getElementById("exposure-form").requestSubmit()');
 await waitFor('controlsData.commands.some(c=>c.kind==="exposure_set" && c.payload.ticker==="LKOH" && c.payload.sensitivity==="1.5" && c.payload.note==="половина выручки")');
 // The stand measured the reaction: a hint only where it disagrees with the set value
 // (oil, just set to 1.5, is close to the measured 1.0).
 assert(await evaluate('(()=>{const oil=document.querySelector(\'[data-exposure="LKOH|oil"] .measured-hint\'), ruble=document.querySelector(\'[data-exposure="LKOH|ruble"] .measured-hint\'), sanctions=document.querySelector(\'[data-exposure="LKOH|sanctions"] .measured-hint\'), rate=document.querySelector(\'[data-exposure="SBER|key_rate"] .measured-hint\');return oil===null && ruble?.textContent==="≈+0,5" && sanctions===null && rate?.textContent==="≈-1,5"})()'),'Measured hints wrong: '+await evaluate('[...document.querySelectorAll(".measured-hint")].map(x=>x.closest("td").dataset.exposure+" "+x.textContent).join("; ")'));
 await evaluate('document.querySelector(\'#exposures-table [data-exposure="SBER|key_rate"]\').click()');
 await waitFor('!document.getElementById("exposure-measured").hidden && document.getElementById("exposure-measured").textContent.includes("дней с событиями 11")');
 await evaluate('document.getElementById("exposure-use-measured").click();document.getElementById("exposure-form").requestSubmit()');
 await waitFor('controlsData.commands.some(c=>c.kind==="exposure_set" && c.payload.ticker==="SBER" && c.payload.topic==="key_rate" && c.payload.sensitivity==="-1.5" && c.payload.note.startsWith("по измерению"))');
 await evaluate('document.querySelector(\'[data-exposure-confirm="LKOH"]\').click()');
 await waitFor('controlsData.commands.some(c=>c.kind==="exposure_confirm" && c.payload.ticker==="LKOH")');
 await evaluate('chooseStrategy("ai-news")');
 await waitFor('document.getElementById("ai-panel").hidden===false && document.querySelectorAll("#ai-decisions .ai-decision").length===4');
 assert(await evaluate('(()=>{const t=document.getElementById("ai-decisions").textContent;return t.includes("Дневной лимит расходов на ИИ исчерпан (503 ₽ из 500 ₽)") && t.includes("3,41 ₽") && t.includes("5 запросов, 49 тыс. → 900 токенов")})()'),'AI cost or budget stop not shown: '+await evaluate('document.getElementById("ai-decisions").textContent.slice(0,600)'));
 assert(await evaluate('document.querySelector("#ai-memory .ai-note.inherited")?.textContent.includes("перенесено из предыдущей версии портфеля")'),'Inherited note not marked');
 assert(await evaluate('(()=>{const a=document.querySelector("#ai-decisions .ai-sources li.cited a");return a && a.getAttribute("href")==="#documents/tinvest%3A1001" && a.closest("li").textContent.includes("прочитан полностью")})()'),'Cited document not linked');
 assert(await evaluate('(()=>{const t=document.getElementById("ai-decisions").textContent;return t.includes("HTTP 529") && t.includes("SBER 20%") && t.includes("RSI 55") && t.includes("всплеск новостей") && t.includes("Держать: без сделок") && t.includes("Повод: 4 новостей")})()'),'AI decisions not shown');
 assert(await evaluate('document.getElementById("ai-memory-count").textContent==="2" && document.getElementById("ai-memory").textContent.includes("выход при пробое 270") && document.getElementById("ai-memory").textContent.includes("вывод разбора")'),'AI memory not shown');
 assert(await evaluate('(()=>{const t=document.getElementById("ai-theses").textContent;return document.getElementById("ai-theses-count").textContent==="1" && t.includes("Снижение ставки") && t.includes("цена ≤ 265") && t.includes("дата 2026-12-20") && t.includes("результат +6%") && t.includes("перенесён")})()'),'AI theses not shown: '+await evaluate('document.getElementById("ai-theses").textContent.slice(0,400)'));
 assert(await evaluate('document.getElementById("ai-decisions").textContent.includes("через 1 день: +0,42% против индекса +0,1%") && document.getElementById("ai-decisions").textContent.includes("через 5 дней: -0,3%")'),'AI outcomes not shown');
 assert(await evaluate('(()=>{const links=[...document.querySelectorAll(".ai-sources a")].filter(a=>!a.getAttribute("href").startsWith("#"));return links.length===1 && links[0].target==="_blank" && links[0].rel.includes("noopener") && ![...document.querySelectorAll(".ai-sources a")].some(a=>a.getAttribute("href").startsWith("javascript"))})()'),'Unsafe or missing news links');
 await evaluate('document.getElementById("ai-preview").click()');
 await waitFor('controlsData.commands.some(c=>c.kind==="ai_preview" && c.payload.strategy_id==="fixture-ai") && document.querySelectorAll("#ai-decisions .ai-decision").length===5');
 // The builder keeps the AI block settings and sends them with the portfolio.
 await evaluate('document.getElementById("portfolio-editor").open=true');
 await waitFor('document.querySelector("#portfolio-blocks .ai-params textarea")?.value==="Покупай на хороших новостях"');
 assert(await evaluate('document.querySelector(\'#portfolio-blocks [data-field="ai:model_profile"]\').value==="claude" && document.querySelectorAll(\'#portfolio-blocks [data-field="ai:model_profile"] option\').length===2'),'AI profile select wrong');
 await evaluate('(()=>{const t=document.querySelector("#portfolio-blocks .ai-params textarea");t.value="Только по новостям";t.dispatchEvent(new Event("input",{bubbles:true}));const s=document.querySelector(\'#portfolio-blocks [data-field="ai:model_profile"]\');s.value="local";s.dispatchEvent(new Event("input",{bubbles:true}));const c=document.querySelector(\'#portfolio-blocks [data-field="ai:allow_short"]\');c.checked=true;c.dispatchEvent(new Event("change",{bubbles:true}));const i=document.querySelector(\'#portfolio-blocks [data-field="ai:decision_interval_minutes"]\');i.value="30";i.dispatchEvent(new Event("input",{bubbles:true}))})()');
 await evaluate('document.getElementById("portfolio-form").requestSubmit()');
 await waitFor('controlsData.commands.some(c=>c.kind==="portfolio" && c.strategy_id==="fixture-ai")');
 assert(await evaluate('(()=>{const p=controlsData.commands.find(c=>c.kind==="portfolio" && c.strategy_id==="fixture-ai").payload.blocks[0].strategies[0];return p.kind==="ai-model" && p.params.instructions==="Только по новостям" && p.params.model_profile==="local" && p.params.allow_short===true && p.params.decision_interval_minutes===30})()'),'AI block settings not sent');
 await evaluate('location.hash="#documents"');
 // Documents: collector status, the feed, one document with its other sources.
 await waitFor('document.getElementById("documents").offsetParent!==null && document.querySelectorAll("#documents-list .document-row").length===2');
 assert(await evaluate('(()=>{const t=document.getElementById("collector-status").textContent;return t.includes("Новости T-Invest") && t.includes("+12") && t.includes("ошибка") && document.getElementById("documents-total").textContent==="1\u00a0630"})()'),'Collector status wrong: '+await evaluate('document.getElementById("collector-status").textContent+" | "+document.getElementById("documents-total").textContent'));
 await evaluate('document.querySelector("#documents-list .document-row").click()');
 await waitFor('location.hash==="#documents/tinvest:1001" && document.getElementById("document-view").textContent.includes("Дата закрытия реестра")');
 assert(await evaluate('(()=>{const t=document.getElementById("document-view").textContent;return t.includes("Раскрытие эмитента") && t.includes("kommersant.ru: Сбер рекомендовал дивиденды") && t.includes("Показатель | Значение") && t.includes("дивидендная политика ↑") && t.includes("важность 4")})()'),'Document view wrong: '+await evaluate('document.getElementById("document-view").textContent.slice(0,500)'));
 await evaluate('document.getElementById("documents-q").value="нефть";document.getElementById("documents-form").requestSubmit()');
 await waitFor('document.querySelectorAll("#documents-list .document-row").length===1 && document.getElementById("documents-list").textContent.includes("Brent")');
 await writeFile('target/control-browser-documents.png',Buffer.from((await call('Page.captureScreenshot',{format:'png'})).data,'base64'));
 await evaluate('location.hash="#models"');await pause(300);
 await writeFile('target/control-browser-models.png',Buffer.from((await call('Page.captureScreenshot',{format:'png'})).data,'base64'));
 await evaluate('chooseStrategy("ai-news")');await waitFor('document.getElementById("ai-panel").hidden===false');
 await evaluate('document.querySelector("#ai-decisions .ai-decision:not(.failed)").open=true;document.getElementById("ai-panel").scrollIntoView({behavior:"instant"})');await pause(300);
 await writeFile('target/control-browser-ai.png',Buffer.from((await call('Page.captureScreenshot',{format:'png'})).data,'base64'));
 // Real accounts: status, gate, account survey, promotion with typed confirmation, emergency stop.
 await evaluate('location.hash="#live"');
 await waitFor('document.getElementById("live").offsetParent!==null && document.querySelectorAll("#live-gate tbody tr").length===1');
 assert(await evaluate('document.querySelectorAll("#live-status .live-check.ok").length===2 && document.getElementById("live-status").textContent.includes("TINKOFF_TOKEN_PRODUCTION подключён") && !document.getElementById("live-status").textContent.includes("ALLOW")'),'Live status wrong');
 assert(await evaluate('document.querySelectorAll("#live-accounts tbody tr").length===2 && document.querySelectorAll("#promote-account option").length===1 && document.getElementById("promote-account").value==="2000999999"'),'Only empty accounts must be offered');
 assert(await evaluate('document.querySelector("#live-promote-form [type=submit]").disabled && document.getElementById("promote-check").textContent.includes("4 цифры")'),'Promotion without confirmation allowed');
 const input=async(id,value)=>evaluate(`(()=>{const e=document.getElementById(${JSON.stringify(id)});e.value=${JSON.stringify(value)};e.dispatchEvent(new Event("input",{bubbles:true}))})()`);
 await input('promote-budget','60000');await input('promote-confirm','9999');
 assert(await evaluate('document.querySelector("#live-promote-form [type=submit]").disabled && document.getElementById("promote-check").textContent.includes("бюджет больше")'),'Budget above free cash allowed');
 await input('promote-budget','30000');
 await waitFor('!document.querySelector("#live-promote-form [type=submit]").disabled');
 assert(await evaluate('document.getElementById("promote-kind").value==="live-pairs-trading"'),'Default real portfolio name missing');
 await evaluate('document.getElementById("live-promote-form").requestSubmit()');
 await waitFor('controlsData.commands.some(c=>c.kind==="live_promote")');
 assert(await evaluate('(()=>{const p=controlsData.commands.find(c=>c.kind==="live_promote").payload;return p.account_id==="2000999999" && p.budget==="30000.00" && p.confirm==="9999" && p.source_strategy_id==="fixture" && p.daily_loss_percent==="2"})()'),'Promotion payload wrong');
 assert(await evaluate('document.querySelector(".account-card.live")!==null && document.getElementById("account-count").textContent.startsWith("2 счёта песочницы") && document.getElementById("account-count").textContent.includes("реальный счёт: 1")'),'Real account not separated from virtual rubles');
 await evaluate('chooseStrategy("live-pairs")');
 await waitFor('document.getElementById("live-banner").hidden===false');
 assert(await evaluate('document.querySelector("#strategy-rows tr.live-row .badge.live")!==null'),'Real strategy row not marked');
 assert(await evaluate('document.getElementById("payin-form").hidden && document.getElementById("manual-form").hidden && document.getElementById("portfolio-editor").hidden && document.getElementById("execution-mode").disabled && document.getElementById("execution-mode").value==="confirm" && document.getElementById("schedule-close").disabled'),'Sandbox tools visible on a real account');
 const commandsBefore=await evaluate('controlsData.commands.length');
 await evaluate('document.querySelector(\'[data-command="resume"]\').click()');
 assert(await evaluate(`controlsData.commands.length===${commandsBefore} && document.getElementById("command-message").textContent.includes("4 цифры")`),'Real-account resume sent without confirmation');
 await evaluate('document.getElementById("live-confirm").value="3456";document.querySelector(\'[data-command="resume"]\').click()');
 await waitFor('controlsData.commands.some(c=>c.kind==="resume" && c.strategy_id==="fixture-live" && c.payload.confirm==="3456")');
 await evaluate('location.hash="#live"');await waitFor('document.getElementById("live").offsetParent!==null');await pause(250);
 await writeFile('target/control-browser-live.png',Buffer.from((await call('Page.captureScreenshot',{format:'png'})).data,'base64'));
 await evaluate('document.getElementById("live-halt").click()');
 await waitFor('controlsData.live.halted===true && document.getElementById("live-halt").textContent.includes("Снять") && controlsData.cases[2].enabled===false');
 await evaluate('document.getElementById("live-halt").click()');
 await waitFor('controlsData.live.halted===false');
 await evaluate('chooseStrategy("pairs-trading")');
 await waitFor('document.getElementById("live-banner").hidden===true && !document.getElementById("payin-form").hidden');
 // Back/forward keeps working: the address restores the strategy page.
 await evaluate('history.back()');await waitFor('location.hash==="#live" && document.getElementById("live").offsetParent!==null');
 await evaluate('history.forward()');await waitFor('location.hash==="#strategy/pairs-trading" && document.getElementById("strategy-detail").offsetParent!==null');
 // Dark theme: tokens and generated rules switch together; the choice persists.
 await evaluate('document.getElementById("theme-toggle").click()');
 await waitFor('document.documentElement.dataset.theme==="dark" && localStorage.getItem("dashboard-theme")==="dark"');
 assert(await evaluate('(()=>{const bg=getComputedStyle(document.body).backgroundColor, panel=getComputedStyle(document.querySelector("#strategy-detail")).backgroundColor, ink=getComputedStyle(document.querySelector("h1")).color;const l=c=>{const [r,g,b]=c.match(/\\d+/g).map(Number);return (r+g+b)/3};return l(bg)<50 && l(panel)<60 && l(ink)>180})()'),'Dark theme not applied');
 await pause(250);await writeFile('target/control-browser-dark.png',Buffer.from((await call('Page.captureScreenshot',{format:'png'})).data,'base64'));
 await evaluate('location.hash="#live"');await pause(300);
 await writeFile('target/control-browser-dark-live.png',Buffer.from((await call('Page.captureScreenshot',{format:'png'})).data,'base64'));
 await evaluate('document.getElementById("theme-toggle").click()');
 await waitFor('document.documentElement.dataset.theme==="light" && document.documentElement.dataset.themeChoice==="light"');
 await evaluate('chooseStrategy("pairs-trading")');
 await evaluate('document.getElementById("strategy-detail").scrollIntoView()');await pause(250);
 await writeFile('target/control-browser-desktop.png',Buffer.from((await call('Page.captureScreenshot',{format:'png'})).data,'base64'));
 await call('Emulation.setDeviceMetricsOverride',{width:390,height:844,deviceScaleFactor:1,mobile:true});
 await pause(150);
 assert(await evaluate('document.documentElement.scrollWidth<=window.innerWidth+2'),'Mobile page overflows');
 await writeFile('target/control-browser-mobile.png',Buffer.from((await call('Page.captureScreenshot',{format:'png'})).data,'base64'));
 assert(errors.length===0,'Browser errors: '+errors.join('; '));
 console.log('Browser smoke passed: AI spend in rubles and its limit, commissions and trade band, portfolios and archive, catalog of any type, AI monitor and rating, theses, lessons and outcomes, exposure editor, documents (collector, feed, full text, topics), storage and disk warning, AI spend and budget stop, AI models and decisions, sections, dark theme, sleep notice, real accounts (gate, survey, promotion, confirmation, emergency stop), session hint, operator schedule, runtime limit, budget journal, portfolio builder, log details, strategy journal, current activity, trade markers, secure commands, duplicate/repeat IDs, manual lots, budget, pay-in, plans, catalog, fast candles, ranking, mobile');
} finally { if(socket)socket.close();chrome.kill();await pause(500);await rm(profile,{recursive:true,force:true}); }
