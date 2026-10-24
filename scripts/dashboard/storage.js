"use strict";

// Storage of the stand on the overview: free disk space (the Mac disk is the
// scarcest resource), database size, nightly maintenance and backups.
const STORAGE_PLACES={"/run/ai":"Диск Mac","/data":"Том стенда (VM)"};
const bytes=value=>value==null ? "—" : value>=1073741824 ? `${fmt(value/1073741824,1)} ГБ` : `${fmt(value/1048576,value<10485760 ? 1 : 0)} МБ`;
const DELETED_NAMES={risk_check_events:"проверок риска",performance_samples:"замеров доходности",ai_usage:"записей расходов",documents:"документов",document_bodies:"текстов документов",embeddings:"векторов"};

document.querySelector(".chart-panel").insertAdjacentHTML("afterend",`<section id="storage" class="panel"><div class="section-heading"><div><h2>Хранилище</h2><p id="storage-meta" class="muted">Место на диске, база, обслуживание и резервные копии</p></div></div><div id="storage-content"></div></section>`);

function storageWarnings() {
  const disk=data?.storage?.disk;
  if(!disk || disk.level==="ok" || !disk.level)return [];
  const place=STORAGE_PLACES[disk.path] || disk.path;
  return [disk.level==="critical"
    ? `Свободного места почти нет: ${bytes(disk.free_bytes)} (${place}). Сбор документов и резервные копии остановлены; освободите место — торговля продолжается.`
    : `Мало свободного места: ${bytes(disk.free_bytes)} (${place}). Освободите место на диске, иначе стенд остановит сбор документов.`];
}

function renderStorage() {
  const s=data?.storage || {}, disk=s.disk || {}, upkeep=s.maintenance || {}, copies=s.backups || {};
  const level={ok:["Достаточно","good"],warn:["Мало","warn"],critical:["Критически мало","error"]}[disk.level] || ["Не проверено","waiting"];
  const readings=(disk.readings || []).map(r=>`<li>${esc(STORAGE_PLACES[r.path] || r.path)}: свободно <strong>${bytes(r.free_bytes)}</strong> из ${bytes(r.total_bytes)}</li>`).join("");
  const deleted=Object.entries(upkeep.deleted || {}).filter(([,n])=>Number(n)>0).map(([k,n])=>`${fmt(n)} ${DELETED_NAMES[k] || k}`).join(", ");
  const status=copies.status;
  const files=(copies.files || []).map(f=>`<li>${esc(datetime(f.at))} МСК · ${bytes(f.bytes)}</li>`).join("");
  $("storage-content").innerHTML=`<div class="storage-grid">
    <div><h3>Диск ${badge(level[0],level[1])}</h3>${readings ? `<ul class="plain-list">${readings}</ul>` : '<p class="muted-text">Наблюдатель ещё не проверял место.</p>'}<p class="detail-note">База данных: ${bytes(disk.database_bytes ?? upkeep.database_bytes)}${disk.checked_at ? ` · проверено ${esc(ageText(disk.checked_at))}` : ""}</p></div>
    <div><h3>Обслуживание</h3>${upkeep.at ? `<p>Последнее: ${esc(datetime(upkeep.at))} МСК${upkeep.seconds!=null ? ` · ${fmt(upkeep.seconds,1)} с` : ""}</p><p class="detail-note">${deleted ? `Удалено устаревшее: ${esc(deleted)}.` : "Удалять было нечего."}${(upkeep.rotated_logs || []).length ? ` Журналы сжаты: ${esc(upkeep.rotated_logs.join(", "))}.` : ""}</p>` : '<p class="muted-text">Ещё не выполнялось: каждую ночь в 03:30 МСК.</p>'}${upkeep.error ? `<p class="warning">Ошибка: ${esc(upkeep.error)}; повтор через час.</p>` : ""}</div>
    <div><h3>Резервные копии</h3>${status ? `<p class="${status.ok ? "" : "warning"}">${status.ok ? "Последняя копия сделана" : "Копия не сделана"}: ${esc(datetime(status.at))} МСК${status.ok ? "" : ` — ${esc(status.message)}`}</p>` : '<p class="muted-text">Копий пока нет: служба backup делает их каждую ночь.</p>'}${files ? `<ul class="plain-list">${files}</ul>` : ""}<p class="detail-note">Хранятся последние копии в томе стенда (/data/backups).</p>${offsiteHtml(copies.offsite)}</div>
  </div>`;
}

// Encrypted copies in an S3 bucket (off the computer); off until the settings file exists.
function offsiteHtml(o) {
  if(!o || !o.at)return '<p class="detail-note">Копии вне компьютера выключены: заполните docker/local/backup/s3.env (см. «Резервные копии» в docs/DOCKER.ru.md).</p>';
  if(!o.ok)return `<p class="warning">Копия вне компьютера не отправлена: ${esc(o.error || "ошибка")} (${esc(datetime(o.at))} МСК)</p>`;
  return `<p>Вне компьютера: последняя копия ${esc(o.latest || "—")} в бакете ${esc(o.bucket || "")}, проверено ${esc(datetime(o.at))} МСК${o.removed ? ` · удалено устаревших: ${fmt(o.removed)}` : ""}</p>`;
}
