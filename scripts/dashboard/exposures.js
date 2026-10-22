"use strict";

// How the AI model links events to instruments without a mention: each instrument
// has a sensitivity to market topics (−2…+2: how good a move of the topic up is
// for the stock). The model drafts it; the operator edits and confirms it here.
document.getElementById("models-list").insertAdjacentHTML("afterend",`<div id="exposures" class="exposures"><h3>Чувствительность бумаг к темам рынка</h3><p class="detail-note">Через неё события влияют на бумагу без упоминания: «санкции на экспорт нефти» — вниз для нефтяников, «рост Brent» — вверх. Черновик профиля пишет модель при первом решении по бумаге; ваши правки модель не меняет. +2 — бумаге сильно хорошо, когда тема движется вверх, −2 — сильно плохо.</p><div id="exposures-table"></div>
  <form id="exposure-form" class="exposure-form" hidden><strong id="exposure-target"></strong><label>Чувствительность<input id="exposure-value" type="number" min="-2" max="2" step="0.1"></label><label class="grow">Пояснение<input id="exposure-note" maxlength="300"></label><span id="exposure-measured" class="subtext" hidden></span><button class="button" type="button" id="exposure-use-measured" hidden>Подставить измеренное</button><button class="button primary" type="submit">Сохранить</button><button class="button" type="button" id="exposure-remove">Убрать тему</button><button class="button" type="button" id="exposure-cancel">Отмена</button></form></div>`);
let exposureEdit=null;

// Measured reaction (stand calibration): the stock's move on days with events of the
// topic, per unit of the topic signal, above the index (whole move for market-wide topics).
function measuredText(m) {
  const effect=Number(m.effect), sign=effect>0 ? "+" : "";
  return `за 120 дней: ${sign}${fmt(effect,2)}% на единицу сигнала темы, дней с событиями ${m.observations}, корреляция ${fmt(Number(m.correlation || 0),2)}; предлагается ${Number(m.suggested)>0 ? "+" : ""}${fmt(Number(m.suggested),1)}`;
}

// The measurement disagrees with what is set: a hint worth the operator's look.
function measuredDiffers(e,m) {
  if(!m)return false;
  const suggested=Number(m.suggested);
  return e ? Math.abs(suggested-Number(e.value))>=1 || (suggested!==0 && Math.sign(suggested)!==Math.sign(Number(e.value))) : suggested!==0;
}

function exposureCell(profile,topic) {
  const e=profile.exposures?.[topic], m=profile.measured?.[topic];
  const hint=measuredDiffers(e,m) ? `<span class="measured-hint" title="Измерено ${esc(measuredText(m))}">≈${Number(m.suggested)>0 ? "+" : ""}${fmt(Number(m.suggested),1)}</span>` : "";
  if(!e)return `<td class="exposure-cell empty" data-exposure="${esc(profile.ticker)}|${esc(topic)}" title="Задать">·${hint}</td>`;
  const value=Number(e.value), tone=value>0 ? "up" : value<0 ? "down" : "";
  const level=Math.min(4,Math.max(1,Math.ceil(Math.abs(value)*2)));
  return `<td class="exposure-cell ${tone} level${level} ${e.origin==="operator" ? "operator" : ""}" data-exposure="${esc(profile.ticker)}|${esc(topic)}" title="${esc(e.note || "")}${e.origin==="operator" ? " · задано вами" : ""}">${value>0 ? "+" : ""}${fmt(value,1)}${hint}</td>`;
}

function renderExposures() {
  const profiles=controlsData?.exposures || [], topics=controlsData?.topics || {};
  if(!profiles.length){$("exposures-table").innerHTML=empty("Профилей пока нет","Они появятся после первого решения модели по бумагам блока.");return;}
  const used=Object.keys(topics).filter(t=>profiles.some(p=>p.exposures?.[t] || measuredDiffers(p.exposures?.[t],p.measured?.[t])));
  const columns=used.length ? used : Object.keys(topics).slice(0,8);
  const html=`<div class="table-wrap exposures-wrap"><table class="exposures-matrix"><thead><tr><th>Бумага</th>${columns.map(t=>`<th title="${esc(topics[t])}">${esc(topics[t])}</th>`).join("")}<th>Профиль</th></tr></thead><tbody>${profiles.map(p=>`<tr><td><strong>${esc(p.ticker)}</strong><span class="subtext">${esc(p.sector || p.name || "")}</span></td>${columns.map(t=>exposureCell(p,t)).join("")}<td class="exposure-status">${p.draft_error && !p.drafted_at ? `<span class="warning" title="${esc(p.draft_error)}">черновик не удался</span>` : !p.drafted_at ? '<span class="muted-text">черновик готовится</span>' : p.confirmed_at ? badge("подтверждён","good") : `<button type="button" class="button small" data-exposure-confirm="${esc(p.ticker)}">Подтвердить</button>`}<button type="button" class="link-button" data-exposure-redraft="${esc(p.ticker)}" title="Модель напишет черновик заново; ваши значения останутся">заново</button>${p.note ? `<span class="subtext" title="${esc(p.note)}">${esc(p.note.slice(0,80))}${p.note.length>80 ? "…" : ""}</span>` : ""}</td></tr>`).join("")}</tbody></table></div><p class="detail-note">Показаны темы, заданные хотя бы для одной бумаги. Нажмите на клетку, чтобы изменить значение; на «·» — чтобы задать. «≈» — стенд измерил реакцию бумаги на события темы, и она расходится с заданным значением (подробности — в подсказке и в форме правки). Все темы: ${Object.values(topics).map(esc).join(", ")}.</p>
    <div class="exposure-add"><label>Добавить тему<select id="exposure-topic">${Object.entries(topics).filter(([t])=>!columns.includes(t)).map(([t,n])=>`<option value="${esc(t)}">${esc(n)}</option>`).join("")}</select></label><label>Бумаге<select id="exposure-ticker">${profiles.map(p=>`<option>${esc(p.ticker)}</option>`).join("")}</select></label><button type="button" class="button small" id="exposure-add">Задать</button></div>`;
  if($("exposures-table").dataset.html!==html){$("exposures-table").innerHTML=html;$("exposures-table").dataset.html=html;}
}

function openExposure(ticker,topic) {
  const profile=(controlsData?.exposures || []).find(p=>p.ticker===ticker), current=profile?.exposures?.[topic];
  exposureEdit={ticker,topic};
  $("exposure-target").textContent=`${ticker} · ${controlsData?.topics?.[topic] || topic}`;
  $("exposure-value").value=current ? Number(current.value) : "";
  $("exposure-note").value=current?.note || "";
  $("exposure-remove").hidden=!current;
  const measured=profile?.measured?.[topic];
  $("exposure-measured").hidden=!measured;
  $("exposure-measured").textContent=measured ? `Измерено ${measuredText(measured)}` : "";
  $("exposure-use-measured").hidden=!measured;
  $("exposure-form").hidden=false;
  $("exposure-value").focus();
}

$("exposures").addEventListener("click",e=>{
  const cell=e.target.closest("[data-exposure]");
  if(cell){const [ticker,topic]=cell.dataset.exposure.split("|");openExposure(ticker,topic);return;}
  const confirm=e.target.closest("[data-exposure-confirm]");
  if(confirm){issue("exposure_confirm",{ticker:confirm.dataset.exposureConfirm},true);return;}
  const redraft=e.target.closest("[data-exposure-redraft]");
  if(redraft){issue("exposure_redraft",{ticker:redraft.dataset.exposureRedraft},true);return;}
  if(e.target.id==="exposure-add")openExposure($("exposure-ticker").value,$("exposure-topic").value);
});
$("exposure-form").addEventListener("submit",e=>{
  e.preventDefault();
  if(!exposureEdit || $("exposure-value").value==="")return;
  issue("exposure_set",{...exposureEdit,sensitivity:$("exposure-value").value,note:$("exposure-note").value},true);
  $("exposure-form").hidden=true;
});
$("exposure-remove").addEventListener("click",()=>{if(exposureEdit){issue("exposure_set",{...exposureEdit,sensitivity:null,note:""},true);$("exposure-form").hidden=true;}});
$("exposure-use-measured").addEventListener("click",()=>{
  const profile=(controlsData?.exposures || []).find(p=>p.ticker===exposureEdit?.ticker), m=profile?.measured?.[exposureEdit?.topic];
  if(!m)return;
  $("exposure-value").value=Number(m.suggested);
  $("exposure-note").value=`по измерению: ${Number(m.effect)>0 ? "+" : ""}${fmt(Number(m.effect),2)}% на единицу сигнала, ${m.observations} дн.`;
});
$("exposure-cancel").addEventListener("click",()=>{$("exposure-form").hidden=true;exposureEdit=null;});
