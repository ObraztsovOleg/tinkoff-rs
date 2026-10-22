"use strict";

// Documents the AI model reads: wire news with full texts, issuer disclosures,
// articles, exchange and Bank of Russia notices. Collector status, a searchable
// feed and the full text of one document (#documents/<id>).
const DOC_KINDS={news:"Лента агентства",article:"Статья",disclosure:"Раскрытие эмитента",press_release:"Пресс-релиз",report:"Отчётность",exchange:"Мосбиржа",regulator:"Банк России"};
const DOC_SOURCES={interfax:"Интерфакс",ria:"РИА Новости",tass:"ТАСС",eDisclosure:"Раскрытие информации",moex:"Мосбиржа",cbr:"Банк России"};
const COLLECTOR_NAMES={tinvest:"Новости T-Invest",rss:"Деловые ленты",moex:"Мосбиржа",cbr:"Банк России",embeddings:"Векторы для поиска",tagging:"Разметка",exposures:"Профили бумаг",retention:"Очистка"};
const docSource=s=>DOC_SOURCES[s] || s;
let docQuery="", docRequest=0;

document.getElementById("models").insertAdjacentHTML("afterend",`<section id="documents" class="panel"><div class="section-heading"><div><h2>Документы <span id="documents-total" class="count-pill">0</span></h2><p id="documents-meta" class="muted">Что читает ИИ-модель: новости, раскрытие эмитентов, статьи, сообщения биржи и Банка России</p></div></div>
  <div id="collector-status" class="collector-status"></div>
  <form id="documents-form" class="documents-form"><label class="sr-only" for="documents-q">Поиск</label><input id="documents-q" type="search" placeholder="Поиск: «дивиденды», «санкции нефть», «ключевая ставка»" maxlength="200">
  <label class="sr-only" for="documents-kind">Вид</label><select id="documents-kind"><option value="">Все виды</option>${Object.entries(DOC_KINDS).map(([k,v])=>`<option value="${k}">${v}</option>`).join("")}</select>
  <label class="sr-only" for="documents-source">Источник</label><select id="documents-source"><option value="">Все источники</option></select>
  <label class="sr-only" for="documents-ticker">Тикер</label><input id="documents-ticker" placeholder="Тикер" maxlength="12" size="7">
  <label class="sr-only" for="documents-days">Период</label><select id="documents-days"><option value="1">Сутки</option><option value="3" selected>3 дня</option><option value="7">Неделя</option><option value="30">Месяц</option><option value="365">Год</option></select>
  <button class="button primary" type="submit">Найти</button></form>
  <div class="documents-columns"><div id="documents-list" class="documents-list"></div><article id="document-view" class="document-view"></article></div></section>`);

function renderCollector() {
  const rows=controlsData?.collector || [], docs=controlsData?.documents;
  if(!docs)return;
  $("documents-total").textContent=fmt(docs.total);
  const kinds=Object.entries(docs.kinds || {}).map(([k,n])=>`${DOC_KINDS[k] || k}: ${fmt(n)}`).join(" · ");
  $("documents-meta").textContent=`За сутки: ${fmt(docs.day)}${kinds ? ` (${kinds})` : ""} · последний получен ${docs.newest ? ageText(docs.newest) : "—"}`;
  const stale=docs.newest && age(docs.newest)>45*60;
  $("collector-status").innerHTML=(stale ? `<p class="warning">Сборщик давно не получал документов: проверьте журнал logs/collector.log. Модель получит предупреждение и будет искать по старым данным.</p>` : "")+(rows.length ? rows.map(r=>`<span class="collector-source ${r.last_error ? "failed" : ""}" title="${esc(typeof aiError==="function" ? aiError(r.last_error || "") : r.last_error || "")}"><strong>${esc(COLLECTOR_NAMES[r.source] || r.source)}</strong> ${r.last_success ? esc(ageText(r.last_success)) : "ещё не было"}${r.added_last ? ` · +${fmt(r.added_last)}` : ""} · всего ${fmt(r.added_total)}${r.last_error ? ` · ошибка${/credit balance/i.test(r.last_error) ? ": нет средств на счёте Anthropic" : ""}` : ""}</span>`).join("") : '<span class="muted-text">Сборщик ещё не запускался: он включается, когда работает портфель с ИИ-моделью.</span>');
  const select=$("documents-source"), current=select.value;
  const sources=docs.sources || [];
  if(select.options.length!==sources.length+1){select.innerHTML=`<option value="">Все источники</option>`+sources.map(s=>`<option value="${esc(s)}">${esc(docSource(s))}</option>`).join("");select.value=current;}
}

async function loadDocuments() {
  const params=new URLSearchParams({q:$("documents-q").value.trim(),kind:$("documents-kind").value,source:$("documents-source").value,ticker:$("documents-ticker").value.trim(),days:$("documents-days").value});
  const query=params.toString(), request=++docRequest;
  $("documents-list").innerHTML='<p class="muted-text">Ищем…</p>';
  try{
    const r=await fetch(`/api/documents?${query}`,{cache:"no-store",signal:AbortSignal.timeout(15000)});
    const result=await r.json();
    if(!r.ok)throw Error(result.error || "Поиск недоступен");
    if(request!==docRequest)return;
    docQuery=query;
    $("documents-list").innerHTML=result.documents.length ? result.documents.map(d=>`<button type="button" class="document-row ${d.priority || d.importance>=4 ? "important" : ""} ${d.status==="skipped" ? "skipped" : ""}" data-document="${esc(d.id)}"><span class="subtext">${esc(datetime(d.published_at))} МСК · ${esc(DOC_KINDS[d.kind] || d.kind)} · ${esc(docSource(d.source))}${d.sources>1 ? ` +${d.sources-1}` : ""}${(d.tickers || []).length ? " · "+esc(d.tickers.join(", ")) : ""}${d.importance ? ` · важность ${d.importance}` : ""}${d.status==="skipped" ? " · не о рынке" : ""}</span><strong>${esc(d.title)}</strong><span class="document-preview">${esc(d.preview || "")}</span></button>`).join("") : empty("Ничего не найдено","Измените запрос, вид или период.");
  }catch(error){if(request===docRequest)$("documents-list").innerHTML=`<p class="warning">${esc(error.message)}</p>`;}
}

async function openDocument(id) {
  $("document-view").innerHTML='<p class="muted-text">Загружаем документ…</p>';
  try{
    const r=await fetch(`/api/document?id=${encodeURIComponent(id)}`,{cache:"no-store",signal:AbortSignal.timeout(15000)});
    const d=await r.json();
    if(!r.ok)throw Error(d.error || "Документ недоступен");
    const link=safeLink(d.url);
    const tables=(d.tables || []).map(t=>`<pre class="document-table">${esc(t)}</pre>`).join("");
    const same=(d.same_event || []).map(x=>`<li><a href="#documents/${esc(x.id)}">${esc(docSource(x.source))}: ${esc(x.title)}</a></li>`).join("");
    $("document-view").innerHTML=`<div class="subtext">${esc(datetime(d.published_at))} МСК · ${esc(DOC_KINDS[d.kind] || d.kind)} · ${esc(docSource(d.source))}${(d.tickers || []).length ? " · "+esc(d.tickers.join(", ")) : ""}${d.priority ? " · важная" : ""}</div><h3>${esc(d.title)}</h3>
      ${d.summary ? `<p class="document-summary">${esc(d.summary)}</p>` : ""}${(d.topics || []).length ? `<p class="document-topics">${d.topics.map(t=>`<span class="topic-chip ${t.direction>0 ? "up" : t.direction<0 ? "down" : ""}">${esc(controlsData?.topics?.[t.topic] || t.topic)} ${t.direction>0 ? "↑" : t.direction<0 ? "↓" : "·"}${"●".repeat(t.strength || 1)}</span>`).join("")}${d.importance ? ` <span class="subtext">важность ${d.importance}${d.event_type ? " · "+esc(d.event_type) : ""}</span>` : ""}</p>` : ""}${d.lead ? `<p class="document-lead">${esc(d.lead)}</p>` : ""}
      <div class="document-body">${d.body ? esc(d.body).split("\n").map(p=>`<p>${p}</p>`).join("") : '<p class="muted-text">Полного текста нет: источник дал только анонс.</p>'}</div>${tables}
      ${same ? `<h4>Это же событие в других источниках</h4><ul class="plain-list">${same}</ul>` : ""}
      ${link ? `<p><a href="${esc(link)}" target="_blank" rel="noopener noreferrer">Оригинал ↗</a></p>` : ""}`;
    for(const row of $("documents-list").querySelectorAll(".document-row"))row.classList.toggle("selected",row.dataset.document===id);
  }catch(error){$("document-view").innerHTML=`<p class="warning">${esc(error.message)}</p>`;}
}

// Called by the section router: the list loads when the section opens.
function showDocuments(id) {
  renderCollector();
  if(!docQuery && !$("documents-list").children.length)loadDocuments();
  if(id)openDocument(id);
}

$("documents-form").addEventListener("submit",e=>{e.preventDefault();loadDocuments();});
$("documents-list").addEventListener("click",e=>{const row=e.target.closest("[data-document]");if(row)location.hash=`#documents/${row.dataset.document}`;});
