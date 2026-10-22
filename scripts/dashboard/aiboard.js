"use strict";

// AI models at a glance (#models): what each AI portfolio's model is doing now and
// how the models compare by the results of their decisions.
// A model that thinks longer than this is reported as interrupted (process restarted).
const AI_THINK_STALE_MS=15*60*1000;

function aiNowRows() {
  const byStrategy=new Map((controlsData?.ai_now || []).map(r=>[r.strategy_id,r]));
  return (data?.cases || []).filter(c=>isAiCase(c) && (typeof isCurrentCase!=="function" || isCurrentCase(c))).map(c=>{
    const row=byStrategy.get(c.strategy_id) || {}, activity=row.activity, last=row.last, today=row.today || {};
    const block=(c.blocks || []).flatMap(b=>b.strategies).find(s=>s.kind==="ai-model");
    const profile=block?.params?.model_profile || controlsData?.ai_models?.default;
    const model=(controlsData?.ai_models?.models || []).find(m=>m.name===profile)?.model || last?.model;
    let state, type="waiting";
    if(!c.running){state=c.contour==="production" ? "Портфель выключен" : "Процесс не запущен";type="warn";}
    else if(activity?.state==="thinking"){
      const since=Date.parse(activity.started_at);
      if(Date.now()-since>AI_THINK_STALE_MS){state=`Запрос прерван (${datetime(activity.started_at)})`;type="warn";}
      else {state=`Думает с ${time(activity.started_at)} · ${AI_TRIGGERS[activity.trigger] || activity.trigger}`;type="";}
    } else if(last?.status==="ok")state=`Ждёт повода · последнее решение ${ageText(last.created_at)}`;
    else if(last)state="Последний запрос не удался";
    else state="Решений ещё не было";
    const lastText=!last ? "—" : last.status==="ok" ? (last.hold ? "держать без сделок" : (last.weights || []).map(w=>`${w.ticker} ${fmt(w.weight_percent,1)}%`).join(", ") || "всё в деньгах") : aiError(last.error);
    return `<tr><td><a class="strategy-link" href="#strategy/${encodeURIComponent(c.kind)}">${esc(displayName(c))}</a>${c.contour==="production" ? ' <span class="badge live">РЕАЛЬНЫЙ</span>' : ""}<span class="subtext">${esc(profile || "профиль по умолчанию")}${model ? ` · ${esc(model)}` : ""}</span></td><td>${badge(state,type)}</td><td>${last ? `${datetime(last.created_at)} · ${esc(AI_TRIGGERS[last.trigger] || last.trigger || "")}<span class="subtext">${esc(lastText)}</span>` : "—"}</td><td class="num">${fmt(today.decisions || 0)}${today.failures ? `<span class="subtext warning">сбоев: ${fmt(today.failures)}</span>` : ""}</td><td class="num">${rub(today.cost_rub)}</td></tr>`;
  });
}

function outcomeText(h) {
  if(!h?.n)return '<span class="muted-text">нет итогов</span>';
  const excess=Number(h.excess);
  return `${fmt(Number(h.beat)/Number(h.n)*100,0)}% лучше индекса<span class="subtext ${excess>0 ? "good" : excess<0 ? "warning" : ""}">в среднем ${excess>0 ? "+" : ""}${fmt(excess,2)} п.п. · решений ${fmt(h.n)}</span>`;
}

function aiRatingRows() {
  const runs=controlsData?.ranking?.history || [];
  // Requests that failed before any model answered have no model to rate; they show in the monitor.
  const rows=(controlsData?.ai_rating || []).filter(r=>r.model).map(r=>{
    // Portfolio result of the model: the latest run of each of its strategies.
    const results=(r.strategies || []).map(id=>runs.find(x=>x.strategy_id===id)).filter(x=>x?.return_percent!=null);
    const result=results.length ? results.reduce((sum,x)=>sum+x.return_percent,0)/results.length : null;
    // A model without a single answered decision gets no place.
    const score=Number(r.decisions) ? [r.h20?.n ? Number(r.h20.excess) : null, r.h5?.n ? Number(r.h5.excess) : null, result].find(v=>v!=null && Number.isFinite(v)) : null;
    return {...r,result,score};
  });
  rows.sort((a,b)=>(b.score ?? -Infinity)-(a.score ?? -Infinity) || Number(b.decisions)-Number(a.decisions));
  return rows.map((r,index)=>`<tr><td class="num">${r.score!=null ? index+1 : "—"}</td><td><strong>${esc(r.model || "модель не ответила")}</strong><span class="subtext">${esc(r.profiles || "")} · портфелей: ${fmt((r.strategies || []).length)}</span></td><td>${outcomeText(r.h5)}</td><td>${outcomeText(r.h20)}</td><td class="num">${r.result!=null ? `${r.result>0 ? "+" : ""}${fmt(r.result,2)}%` : "—"}</td><td class="num">${fmt(r.decisions)}${Number(r.failures) ? `<span class="subtext warning">сбоев: ${fmt(r.failures)}</span>` : ""}<span class="subtext">держать: ${fmt(r.holds)}</span></td><td class="num">${rub(r.cost_rub)}<span class="subtext">${Number(r.decisions) ? `${rub(Number(r.cost_rub || 0)/Number(r.decisions))} за решение` : ""}</span></td></tr>`);
}

function renderAiBoard() {
  if(!$("ai-board") || !data)return;
  const now=aiNowRows(), rating=aiRatingRows();
  const html=`<div class="ai-board"><h3>Что делают модели сейчас</h3>${now.length ? table(["Портфель и модель","Сейчас","Последнее решение","Решений сегодня","Расход сегодня"],now) : empty("ИИ-портфелей нет","Добавьте блок «ИИ-модель» в конструкторе портфеля.")}
    <h3>Рейтинг моделей</h3><p class="detail-note">Место — по среднему перевесу решений над индексом Мосбиржи через 20 торговых дней (пока итогов нет — через 5 дней, затем по результату портфелей). «Лучше индекса» — доля решений, после которых доли модели принесли больше индекса. Доходность решений — после комиссии за перекладку (оборот × ставка контура).</p>${rating.length ? table(["#","Модель","Через 5 дней","Через 20 дней","Портфели","Решения","Расход"],rating) : empty("Решений ещё нет","Рейтинг появится после первых решений моделей.")}</div>`;
  if($("ai-board").dataset.html!==html){$("ai-board").innerHTML=html;$("ai-board").dataset.html=html;}
}
