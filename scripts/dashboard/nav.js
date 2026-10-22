"use strict";

// One page per topic: the sidebar chooses the section, the address keeps it
// (#overview, #strategies, #strategy/<name>, #activity, #accounts, #live, ...).
const VIEWS={overview:["◫","Обзор"],portfolios:["▣","Портфели"],strategies:["≋","Стратегии"],activity:["↔","Сделки и позиции"],accounts:["▤","Счета"],live:["●","Реальные счета"],ranking:["★","Рейтинг"],instruments:["⌕","Инструменты"],models:["✦","Модели ИИ"],documents:["▦","Документы"],journal:["☰","Журнал управления"]};
const THEMES={auto:"◐ Тема: как в системе",dark:"☾ Тема: тёмная",light:"☀ Тема: светлая"};
for(const [selector,view] of Object.entries({".session-panel":"overview",".metrics":"overview",".chart-panel":"overview","#storage":"overview","#control-overview":"overview",
  "#strategies":"strategies","#strategy-detail":"strategies","#activity":"activity","#accounts":"accounts","#live":"live","#ranking":"ranking","#instrument-catalog":"instruments","#models":"models","#documents":"documents","#journal":"journal"}))
  document.querySelector(selector)?.setAttribute("data-view",view);
document.querySelector(".sidebar nav").innerHTML=Object.entries(VIEWS).map(([id,[icon,name]])=>`<a href="#${id}" data-nav="${id}"${id==="live" ? ' class="nav-live"' : ""}><span>${icon}</span>${name}</a>`).join("");

function route() {
  const hash=decodeURIComponent(location.hash.slice(1));
  if(hash.startsWith("strategy/"))return {view:"strategies",strategy:hash.slice("strategy/".length)};
  if(hash.startsWith("documents/"))return {view:"documents",strategy:null,document:hash.slice("documents/".length)};
  return {view:VIEWS[hash] ? hash : "overview",strategy:null};
}
function applyView() {
  const {view,strategy,document:documentId}=route();
  // The strategies section shows the list, or one strategy's page from the address.
  if(view==="strategies" && data){
    const want=strategy && data.cases.some(c=>c.kind===strategy) ? strategy : "";
    if($("strategy-filter").value!==want){$("strategy-filter").value=want;renderStrategies();renderActivity();renderAccounts();renderDetail();}
  }
  const detail=view==="strategies" && Boolean(strategy);
  for(const element of document.querySelectorAll("[data-view]"))
    element.classList.toggle("view-hidden",element.dataset.view!==view || (element.id==="strategies" && detail) || (element.id==="strategy-detail" && !detail));
  for(const link of document.querySelectorAll("[data-nav]"))link.classList.toggle("selected",link.dataset.nav===view);
  const c=detail && data && selectedCase();
  $("view-title").textContent=c ? displayName(c) : VIEWS[view][1];
  if(view==="documents" && typeof showDocuments==="function" && applyView.shown!==location.hash){applyView.shown=location.hash;showDocuments(documentId);}
  if(view!=="documents")applyView.shown=null;
  document.title=`${c ? displayName(c) : VIEWS[view][1]} · Strategy Lab`;
}
function renderThemeToggle() {$("theme-toggle").textContent=THEMES[document.documentElement.dataset.themeChoice || "auto"];}
$("theme-toggle").addEventListener("click",()=>{
  const order=["auto","dark","light"], current=document.documentElement.dataset.themeChoice || "auto";
  setThemeChoice(order[(order.indexOf(current)+1)%order.length]);renderThemeToggle();
});
addEventListener("hashchange",()=>{applyView();scrollTo(0,0);});
renderThemeToggle();
applyView();
