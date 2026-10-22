"use strict";

// Commissions (#accounts): the tariff of each contour and the rate per instrument type
// the planner and the AI use — from the tariff or measured from the commissions the
// broker actually charged — and the minimum trade below which positions are not changed.
const COST_KINDS={share:"Акции",etf:"Фонды",bond:"Облигации",futures:"Фьючерсы",currency:"Валюта",default:"Остальное"};
const TARIFFS={sandbox:"песочница (0,05% для всех)",investor:"«Инвестор»",trader:"«Трейдер»",premium:"«Премиум»",unknown:"не определён"};
let bandDirty=false;

$("accounts").insertAdjacentHTML("afterend", `<section class="panel" id="costs" data-view="accounts"><div class="section-heading"><div><h2>Комиссии и порог сделки</h2><p class="muted">Стенд узнаёт тариф у брокера и уточняет ставку по фактически списанным комиссиям; ставка закладывается в расчёт лотов и передаётся ИИ-модели</p></div></div><div id="costs-table"></div><form id="band-form" class="inline-form band-form"><label for="band-percent">Не менять позицию, если изменение меньше, % капитала</label><input id="band-percent" type="number" min="0" max="20" step="0.05" required><button class="button" type="submit">Сохранить</button></form><p class="detail-note">Закрытие позиции и разворот исполняются всегда; новая позиция меньше порога не открывается. При покупке резервируется комиссия. Итоги решений ИИ и его рейтинг считаются после комиссии за перекладку.</p></section>`);

function renderCosts() {
  const rows=controlsData?.commissions || [];
  const contours=[...new Set(rows.map(r=>r.contour))];
  const html=contours.length ? contours.map(contour=>{
    const list=rows.filter(r=>r.contour===contour), tariff=list[0]?.tariff;
    return `<h3>${contour==="production" ? "Реальные счета" : "Песочница"} · тариф ${esc(TARIFFS[tariff] || tariff || "—")}</h3>${table(["Тип","Комиссия","Откуда","Сделок / объём за 90 дней"],list.map(r=>`<tr><td>${esc(COST_KINDS[r.kind] || r.kind)}</td><td class="num">${fmt(Number(r.rate_percent),3)}%</td><td>${r.source==="measured" ? '<span class="good">по фактическим списаниям</span>' : '<span class="muted-text">по тарифу</span>'}</td><td class="num">${fmt(r.trades)} · ${fmt(Number(r.volume_rub),0)} ₽${Number(r.commission_rub) ? `<span class="subtext">списано ${fmt(Number(r.commission_rub),2)} ₽</span>` : ""}</td></tr>`))}<p class="detail-note">Обновлено ${datetime(list[0]?.updated_at)} МСК · по тарифу ставка берётся, пока по типу меньше 5 сделок на 100 000 ₽.</p>`;
  }).join("") : empty("Ставки ещё не определены","Наблюдатель определит тариф и ставки в течение нескольких минут после запуска.");
  if($("costs-table").dataset.html!==html){$("costs-table").innerHTML=html;$("costs-table").dataset.html=html;}
  if(!bandDirty && document.activeElement!==$("band-percent") && controlsData?.min_trade_percent!=null)$("band-percent").value=Number(controlsData.min_trade_percent);
}

$("band-percent").addEventListener("input",()=>{bandDirty=true;});
$("band-form").addEventListener("submit",async e=>{
  e.preventDefault();
  const command=await issue("trade_band",{percent:$("band-percent").value},true);
  if(command?.status==="succeeded")bandDirty=false;
});
