/* Offline retained-report viewer. All report strings are untrusted text. */
(() => {
  'use strict';
  const root = document.getElementById('ff-report-app');
  const source = document.getElementById('failforge-report-data');
  if (!root || !source) return;
  let data;
  try {
    data = JSON.parse(source.textContent);
    if (data.schemaVersion !== 1 || !data.run || !Array.isArray(data.families)) throw new Error('schema');
  } catch (_) {
    root.textContent = 'This report contains unsupported or invalid data.';
    root.setAttribute('role', 'alert');
    return;
  }

  const e = value => String(value ?? '').replace(/[&<>"']/g, ch => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]));
  const familyNames = {classic:'Classic', failure_mode:'Failure mode', language_specific:'Dart-specific'};
  const outcomes = ['KILLED','SURVIVED','INVALID','TIMEOUT','INFRA_ERROR','UNCLASSIFIED','NO_TESTS','CANCELLED'];
  const PAGE_SIZE = 25;
  const FILE_PAGE_SIZE = 30;
  const DIFF_PREVIEW_BYTES = 64 * 1024;
  const SOURCE_CONTEXT_BYTES = 16 * 1024;
  const SOURCE_CONTEXT_LINES = 7;
  const families = data.families;
  const firstFamily = Math.max(0, families.findIndex(f => f.enabled));
  const state = {family:firstFamily, file:null, filter:'all', selected:null, page:0, filePage:0, tab:'change', log:0, runDetails:false, rawDetails:false};
  const copyState = {pending:null,notice:null,manual:null};
  const content = document.createElement('div');
  const announcement = document.createElement('p');
  announcement.className = 'ff-sr';
  announcement.setAttribute('role','status');
  announcement.setAttribute('aria-live','polite');
  const actionAnnouncement = document.createElement('p');
  actionAnnouncement.className = 'ff-sr';
  actionAnnouncement.setAttribute('role','status');
  actionAnnouncement.setAttribute('aria-live','polite');
  actionAnnouncement.setAttribute('aria-atomic','true');
  root.replaceChildren(content,announcement,actionAnnouncement);
  const byFamily = families.map(f => {
    const records = Array.isArray(f.records) ? f.records : [];
    const fileCounts = new Map();
    for (const r of records) {
      const file=String(r.file ?? 'File not recorded');
      fileCounts.set(file,(fileCounts.get(file)||0)+1);
    }
    const files = [...fileCounts.keys()].sort();
    return {records, files, fileCounts};
  });
  const infrastructureErrors = byFamily.reduce((total,family)=>total+family.records.filter(record=>record.outcome==='INFRA_ERROR').length,0);
  resetFilter();

  function number(value) { return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : null; }
  function display(value) { return value === null || value === undefined ? 'Not recorded' : String(value); }
  function duration(value) {
    if (number(value) === null) return 'Duration not recorded';
    if (value < 60) return `${value.toFixed(1)} s`;
    const seconds=Math.round(value);
    return `${Math.floor(seconds / 60)} min ${seconds % 60} s`;
  }
  function shortId(value) { const text = String(value ?? ''); return text.length > 14 ? text.slice(0,12) + '…' : text; }
  function basename(value) { return String(value ?? '').split(/[\\/]/).filter(Boolean).pop() || ''; }
  function counts(records) {
    const result = {total:records.length, killed:0, survived:0, excluded:0, outcomes:Object.fromEntries(outcomes.map(o => [o,0]))};
    for (const r of records) {
      if (Object.hasOwn(result.outcomes,r.outcome)) result.outcomes[r.outcome]++;
      if (r.outcome === 'KILLED') result.killed++;
      else if (r.outcome === 'SURVIVED') result.survived++;
      else result.excluded++;
    }
    return result;
  }
  function resetFilter() {
    state.filter = byFamily[state.family]?.records.some(r => r.outcome === 'SURVIVED') ? 'SURVIVED' : 'all';
  }
  function status(record) { return `<span class="ff-status" data-status="${e(record.outcome)}">${e(record.outcome)}</span>`; }
  function activeFamily() { return families[state.family] || {id:'unknown',enabled:false,records:[]}; }
  function activeRows() { return byFamily[state.family] || {records:[],files:[],fileCounts:new Map()}; }
  function filename(record) { return String(record.file ?? 'File not recorded'); }
  function sourceContext(record) {
    const context=record.sourceContext;
    if (!context || context.version!==1 || typeof context.sourceHash!=='string') return null;
    for (const field of ['line','column','endLine','startLine']) {
      if (!Number.isSafeInteger(context[field]) || context[field]<1) return null;
    }
    if (context.endLine<context.line || context.startLine>context.line || !Array.isArray(context.lines) || context.lines.length===0) return null;
    const lines=context.lines.slice(0,SOURCE_CONTEXT_LINES);
    if (!lines.every(line=>typeof line==='string'&&!/[\r\n]/.test(line)) || !Number.isSafeInteger(context.startLine+lines.length-1) || context.line>=context.startLine+lines.length) return null;
    if (context.truncated!==true && (context.endLine>=context.startLine+lines.length || context.column>lines[context.line-context.startLine].length+1)) return null;
    return {...context,lines,truncated:context.truncated===true||context.lines.length>SOURCE_CONTEXT_LINES};
  }
  function sourceLocation(record) {
    const context=sourceContext(record);
    return filename(record)+(context?':'+context.line:'');
  }
  function sameFile(record) { return state.file === null || filename(record) === state.file; }
  function matches(record) {
    return sameFile(record) && (state.filter === 'all' || (state.filter === 'excluded' ? !['KILLED','SURVIVED'].includes(record.outcome) : record.outcome === state.filter));
  }
  function button(action, value, text, css, extra='',focusKey) {
    return `<button type="button" class="${css}" data-action="${action}" data-value="${e(value)}" data-focus-key="${e(focusKey??action+'-'+value)}" ${extra}>${text}</button>`;
  }
  function fileOptions(files) {
    return `<option value="">All files</option>` + files.map(file => `<option value="${e(file)}" ${state.file===file?'selected':''}>${e(file)}</option>`).join('');
  }
  function render(focusKey) {
    const family = activeFamily(), all = activeRows(), total = counts(all.records);
    const scoped = counts(all.records.filter(sameFile));
    const filtered = all.records.map((record,index)=>({record,index})).filter(item=>matches(item.record));
    const pageCount = Math.max(1,Math.ceil(filtered.length/PAGE_SIZE));
    state.page = Math.min(state.page,pageCount-1);
    const visible = filtered.slice(state.page*PAGE_SIZE,(state.page+1)*PAGE_SIZE);
    if (!filtered.some(item => item.index === state.selected)) state.selected = visible[0]?.index ?? null;
    const selected = state.selected === null ? null : all.records[state.selected];
    const filePageCount = Math.max(1,Math.ceil(all.files.length/FILE_PAGE_SIZE));
    state.filePage = Math.min(state.filePage,filePageCount-1);
    const visibleFiles = all.files.slice(state.filePage*FILE_PAGE_SIZE,(state.filePage+1)*FILE_PAGE_SIZE);
    const selectFiles = state.file && !visibleFiles.includes(state.file) ? [state.file,...visibleFiles] : visibleFiles;
    const complete = data.run.complete === true;
    const scored = complete && family.complete === true && family.score && number(family.score.percent) !== null && number(family.score.denominator) > 0;
    const score = scored ? `${family.score.percent.toFixed(1)}<span class="ff-percent">%</span>` : 'N/A';
    const scoreCaption = !complete ? 'Campaign incomplete' : !family.complete ? 'Family incomplete' : total.killed+total.survived === 0 ? 'No scored mutants' : !scored ? 'Final score not recorded' : `${family.score.killed} / ${family.score.denominator} scored mutants`;
    const statusText = !complete ? (data.run.status==='cancelled'?'Run cancelled':data.run.status==='failed'?'Run failed':'Run incomplete') : infrastructureErrors>0 ? 'Run completed with errors' : ({completed:'Run completed',no_mutants:'No mutants found'}[data.run.status]||'Run completed');
    const configuration = data.configuration && typeof data.configuration === 'object' ? data.configuration : {};
    const projectName = basename(configuration.project) || 'Retained run';
    const exclusions = outcomes.filter(o=>!['KILLED','SURVIVED'].includes(o)&&total.outcomes[o]).map(o=>`${total.outcomes[o]} ${o.toLowerCase()}`).join(' · ');
    const first = filtered.length ? state.page*PAGE_SIZE+1 : 0;
    const last = Math.min((state.page+1)*PAGE_SIZE,filtered.length);

    content.innerHTML = `
      <header class="ff-topbar"><span class="ff-logo" aria-hidden="true">F</span><span class="ff-brand">FailForge</span><span class="ff-slash">/</span><span class="ff-project ff-mono">${e(projectName)}</span><span class="ff-offline">Offline report</span></header>
      <div class="ff-runhead"><div><div class="ff-eyebrow">Mutation report</div><h1>Tests under mutation</h1><div class="ff-runmeta"><span class="ff-mono ff-runid">${e(display(data.run.id))}</span><span>${e(duration(data.run.wallSeconds))}</span><span>${e(display(data.run.completedMutants))}${number(data.run.expectedMutants)!==null?' / '+e(data.run.expectedMutants):''} mutants processed</span></div></div><span class="ff-runstate"><span class="ff-dot ${!complete?'ff-incomplete-dot':infrastructureErrors>0?'ff-warning-dot':''}"></span>${statusText}</span></div>
      ${!complete?'<div class="ff-interrupt" role="note">This campaign is incomplete. Retained results are available, but no family has a final score.</div>':''}
      ${data.run.error?`<div class="ff-runerror" role="note">${e(data.run.error)}</div>`:''}
      <nav class="ff-familybar" aria-label="Mutation families" role="tablist">${families.map((f,index)=>button('family',index,`${e(familyNames[f.id]||f.id)}<span class="ff-count">${f.enabled?byFamily[index].records.length:'Off'}</span>`,'ff-family',`role="tab" id="ff-family-${index}" aria-selected="${index===state.family}" aria-controls="ff-family-content" ${f.enabled?'':'disabled'}`)).join('')}</nav>
      <div id="ff-family-content" role="tabpanel" aria-labelledby="ff-family-${state.family}">
        ${complete&&family.complete===true&&total.outcomes.INFRA_ERROR>0?`<div class="ff-score-warning" role="note"><strong>${total.outcomes.INFRA_ERROR} infrastructure ${total.outcomes.INFRA_ERROR===1?'error':'errors'} excluded.</strong> ${total.killed+total.survived>0?`This family's score uses ${total.killed+total.survived} of ${total.total} mutants (KILLED + SURVIVED).`:'No killed or surviving mutants are available for a numeric score.'}</div>`:''}
        <section class="ff-stats" aria-label="Family totals across all files">
          <div class="ff-stat"><div class="ff-stat-label">${e(familyNames[family.id]||family.id)} score · all files</div><div class="ff-stat-num">${score}</div><div class="ff-stat-caption">${e(scoreCaption)}</div>${scored?`<div class="ff-scorebar" role="img" aria-label="${total.killed} killed, ${total.survived} survived"><span class="ff-killmark" style="width:${total.killed/(total.killed+total.survived)*100}%"></span><span class="ff-survivemark" style="width:${total.survived/(total.killed+total.survived)*100}%"></span></div>`:''}</div>
          <div class="ff-stat"><div class="ff-stat-label ff-statheading"><span class="ff-dot"></span>Survived</div><div class="ff-stat-num">${total.survived}</div><div class="ff-stat-caption">Ready for review</div></div>
          <div class="ff-stat"><div class="ff-stat-label">Killed by tests</div><div class="ff-stat-num">${total.killed}</div><div class="ff-stat-caption">Change detected</div></div>
          <div class="ff-stat"><div class="ff-stat-label">Excluded from score</div><div class="ff-stat-num">${total.excluded}</div><div class="ff-stat-caption">${e(exclusions||'No excluded outcomes')}</div></div>
        </section>
        <div class="ff-body">
          <aside class="ff-sidebar" aria-label="Source files"><div class="ff-sidebar-label">Mutation scope</div>${button('file','all',`All files<span class="ff-count">${total.total}</span>`,'ff-file',`aria-pressed="${state.file===null}"`)}<div class="ff-filefolder">Source files</div>${visibleFiles.map(file=>button('file',all.files.indexOf(file),`<span class="ff-file-label">${e(file)}</span><span class="ff-count">${all.fileCounts.get(file)}</span>`,'ff-file',`aria-pressed="${state.file===file}"`)).join('')}${filePagination(filePageCount,all.files.length)}</aside>
          <div class="ff-content"><div class="ff-file-mobile"><label for="ff-file-select">Source file</label><select id="ff-file-select" data-action="file-select" data-focus-key="file-select">${fileOptions(selectFiles)}</select>${filePagination(filePageCount,all.files.length,'mobile')}</div>
            <div class="ff-toolbar"><div class="ff-filters" role="group" aria-label="Outcome filter">${[['all','All',scoped.total],['SURVIVED','Survived',scoped.survived],['KILLED','Killed',scoped.killed],['excluded','Excluded',scoped.excluded]].map(([value,text,count])=>button('filter',value,`${text}<span class="ff-count">${count}</span>`,'ff-filter',`aria-pressed="${state.filter===value}"`)).join('')}</div><span class="ff-resultcount" role="status" aria-live="polite">Showing ${first}${last!==first?'–'+last:''} of ${filtered.length}${filtered.length!==scoped.total?' · '+scoped.total+' in scope':''}</span></div>
            <div class="ff-split"><section class="ff-list" aria-label="Mutants"><div class="ff-listhead"><span>Mutants</span><span>${state.file===null?'All files':'Selected file'}</span></div>${visible.length?visible.map(({record,index})=>recordButton(record,index)).join(''):`<div class="ff-empty"><strong>${total.total?'No matching mutants':complete?'No mutants found':'No committed mutant results'}</strong>${total.total?'Choose another outcome or source file.':complete?'No candidates were retained in this family.':'The incomplete summary has no retained results to display.'}</div>`}${pageCount>1?`<nav class="ff-pagination" aria-label="Mutant pages">${button('page',state.page-1,'Previous','ff-page',state.page===0?'disabled':'','page-previous')}<span>Page ${state.page+1} of ${pageCount}</span>${button('page',state.page+1,'Next','ff-page',state.page+1===pageCount?'disabled':'','page-next')}</nav>`:''}</section><section class="ff-detail" aria-label="Selected mutant details">${selected?detail(selected):'<div class="ff-empty"><strong>No mutant selected</strong>Select a retained result to inspect its change and evidence.</div>'}</section></div>
          </div>
        </div>
      </div>
      <details class="ff-run-details" data-run-details ${state.runDetails?'open':''}><summary>Run parameters and integrity checks</summary>${runMetadata(configuration)}</details>
      <footer class="ff-footer"><span>Local report · retained evidence</span><span>Family scores are independent</span></footer>`;
    if (focusKey) [...root.querySelectorAll('[data-focus-key]')].find(el=>el.dataset.focusKey===focusKey)?.focus({preventScroll:true});
    announcement.textContent=`${familyNames[family.id]||family.id}: ${filtered.length} matching results. ${selected?'Selected '+selected.mutantId+', '+sourceLocation(selected)+', '+selected.outcome+'.':'No mutant selected.'}`;
    const location=selectedLocation();
    const copyMessage=copyState.notice&&copyState.notice.key===location?.key?copyState.notice.text:'';
    if(actionAnnouncement.textContent!==copyMessage)actionAnnouncement.textContent=copyMessage;
  }
  function filePagination(pageCount,total,location='desktop') {
    return pageCount>1?`<nav class="ff-file-pages" aria-label="Source file pages">${button('file-page',state.filePage-1,'←','ff-page',`aria-label="Previous source files" ${state.filePage===0?'disabled':''}`,'file-'+location+'-previous')}<span>${state.filePage+1} / ${pageCount}<span class="ff-sr"> pages, ${total} files</span></span>${button('file-page',state.filePage+1,'→','ff-page',`aria-label="Next source files" ${state.filePage+1===pageCount?'disabled':''}`,'file-'+location+'-next')}</nav>`:'';
  }
  function clip(value,limit=62) {
    const text=String(value??'');
    const prefix=Array.from(text.slice(0,limit*2+2));
    return prefix.length>limit?prefix.slice(0,limit).join('')+'…':text;
  }
  function recordButton(record,index) {
    const edit=record.edit;
    return button('mutant',index,`<span class="ff-mutant-top"><span class="ff-id">${e(shortId(record.mutantId))}</span>${status(record)}</span><span class="ff-filepath">${e(sourceLocation(record))}</span><div class="ff-operator">${e(display(record.operatorId))}</div>${edit?`<span class="ff-mini-diff"><del>${e(clip(edit.expected)||'∅')}</del><span aria-label="becomes">→</span><ins>${e(clip(edit.replacement)||'∅')}</ins></span>`:'<span class="ff-missing-edit">Change not retained</span>'}`,'ff-mutant',`aria-pressed="${state.selected===index}" aria-label="${e(record.mutantId)}, ${e(sourceLocation(record))}, ${e(record.outcome)}"`);
  }
  function note(record) {
    const descriptions = {
      SURVIVED:['Review this survivor','Tests passed with this change. Review edge cases and whether the mutation is equivalent for valid inputs.'],
      KILLED:['Change detected','Retained test evidence establishes a failure. This mutant contributes to the score.'],
      INVALID:['Invalid mutation','Static validation or compilation rejected the change. This mutant is excluded from the score.'],
      TIMEOUT:['Command limit reached','The outer command was stopped by its time limit. No kill or survival verdict is established.'],
      INFRA_ERROR:['Infrastructure error','Execution, input integrity, capture or cleanup failed. This is not evidence that tests killed the mutant.'],
      UNCLASSIFIED:['Insufficient evidence','The retained evidence does not establish a kill or survival verdict.'],
      NO_TESTS:['No confirmed test execution','No usable test execution was established. This does not establish a lack of code coverage.'],
      CANCELLED:['Execution cancelled','This mutant was interrupted. It is excluded from the score.']
    };
    const [heading,text]=descriptions[record.outcome]||['Result unavailable','A reliable outcome was not retained.'];
    return `<div class="ff-note" data-status="${e(record.outcome)}"><h3>${heading}</h3><p>${text}</p><p class="ff-reason">Reason: <code>${e(display(record.reason))}</code></p></div>`;
  }
  function highlighted(value,other) {
    if (!value) return '<span class="ff-empty-token">∅ (empty)</span>';
    const a=Array.from(String(value)),b=Array.from(String(other));
    let start=0,end=0;
    while(start<a.length&&start<b.length&&a[start]===b[start])start++;
    while(end<a.length-start&&end<b.length-start&&a[a.length-1-end]===b[b.length-1-end])end++;
    return e(a.slice(0,start).join(''))+`<mark>${e(a.slice(start,a.length-end).join(''))}</mark>`+e(end?a.slice(-end).join(''):'');
  }
  function fragmentPreview(value) {
    const text=String(value??'');
    // Bound work before building code-point arrays or DOM. Preserve UTF-8
    // boundaries and explicitly distinguish a preview from the exact edit.
    const prefix=text.slice(0,DIFF_PREVIEW_BYTES);
    const bytes=new TextEncoder().encode(prefix);
    const truncated=prefix.length<text.length||bytes.length>DIFF_PREVIEW_BYTES;
    return {text:truncated?new TextDecoder().decode(bytes.subarray(0,DIFF_PREVIEW_BYTES),{stream:true}):text,truncated};
  }
  function originalSource(record,context) {
    if (!context) {
      const reason=typeof record.sourceContextUnavailable==='string'?record.sourceContextUnavailable:'Source context was not retained.';
      return `<div class="ff-context-missing"><strong>Original source unavailable</strong><p>${e(reason)}</p><p>Export with <code>report --source-root PATH</code> to include matching original source.</p></div>`;
    }
    const lines=[];
    let remaining=SOURCE_CONTEXT_BYTES,truncated=context.truncated;
    for (const line of context.lines) {
      if (remaining===0 && line.length!==0) { truncated=true;break; }
      const prefix=line.slice(0,remaining+2);
      const bytes=new TextEncoder().encode(prefix);
      const clipped=prefix.length<line.length||bytes.length>remaining;
      const text=clipped?new TextDecoder().decode(bytes.subarray(0,remaining),{stream:true}):line;
      remaining-=new TextEncoder().encode(text).length;
      lines.push(text);
      truncated ||= clipped;
    }
    const lastLine=context.startLine+lines.length-1;
    const gutter=String(lastLine).length+2;
    return `<div class="ff-codebox ff-sourcebox"><div class="ff-codehead"><span>Original source${truncated?' · preview':''}</span><span>Line ${context.line} · column ${context.column} (UTF-16)</span></div><div class="ff-source-scroll" role="region" aria-label="Original source, lines ${context.startLine} to ${lastLine}"><div class="ff-source-lines" style="--ff-gutter:${gutter}ch">${lines.map((text,index)=>{
      const line=context.startLine+index,changed=line>=context.line&&line<=context.endLine;
      return `<div class="ff-source-row" data-mutated="${changed}"><span class="ff-line-number" aria-hidden="true">${line}</span><code><span class="ff-sr">Line ${line}${changed?', mutation location':''}: </span>${e(text)||' '}</code></div>`;
    }).join('')}</div></div></div>${truncated?'<p class="ff-bytehint ff-context-limit">Source context is a limited preview.</p>':''}`;
  }
  function detail(record) {
    const edit=record.edit;
    const context=sourceContext(record);
    const before=fragmentPreview(edit?.expected),after=fragmentPreview(edit?.replacement);
    return `<div class="ff-detail-heading"><h2>${e(sourceLocation(record))}</h2>${status(record)}</div><div class="ff-operator">${e(display(record.operatorId))}</div><div class="ff-detailmeta"><span class="ff-mono ff-fullid">${e(display(record.mutantId))}</span>${context?`<span>Column ${context.column} (UTF-16)</span>`:''}${number(record.wallSeconds)!==null?`<span>${e(duration(record.wallSeconds))}</span>`:''}</div>
      ${sourceActions(record,context)}
      <div class="ff-detailtabs" role="tablist" aria-label="Mutant evidence">${[['change','Change'],['tests','Tests'],['metadata','Metadata']].map(([key,text])=>button('tab',key,text,'ff-detailtab',`role="tab" id="ff-${key}-tab" aria-controls="ff-${key}-panel" aria-selected="${state.tab===key}"`)).join('')}</div>
      <div class="ff-panel" role="tabpanel" id="ff-change-panel" aria-labelledby="ff-change-tab" ${state.tab==='change'?'':'hidden'}>${originalSource(record,context)}${edit?`<div class="ff-codebox"><div class="ff-codehead"><span>Mutation patch${before.truncated||after.truncated?' · preview':''}</span><span>− original / + mutant</span></div><div class="ff-diffline ff-minus"><span class="ff-sign" aria-label="Original">−</span><code>${highlighted(before.text,after.text)}</code></div><div class="ff-diffline ff-plus"><span class="ff-sign" aria-label="Mutation">+</span><code>${highlighted(after.text,before.text)}</code></div></div><div class="ff-bytehint">UTF-8 · offset ${e(display(edit.offsetBytes))} bytes · replaced ${e(display(edit.lengthBytes))} bytes</div>${before.truncated||after.truncated?'<p class="ff-bytehint">Change preview is limited to 64 KiB per fragment. The full edit remains in the original run directory.</p>':''}`:'<div class="ff-empty"><strong>Change not retained</strong>The report has no source-bound edit for this result.</div>'}${note(record)}</div>
      <div class="ff-panel" role="tabpanel" id="ff-tests-panel" aria-labelledby="ff-tests-tab" ${state.tab==='tests'?'':'hidden'}>${tests(record)}${note(record)}</div>
      <div class="ff-panel" role="tabpanel" id="ff-metadata-panel" aria-labelledby="ff-metadata-tab" ${state.tab==='metadata'?'':'hidden'}>${metadata(record)}</div>`;
  }
  function kv(pairs) { return `<dl class="ff-kv">${pairs.map(([key,value])=>`<dt>${e(key)}</dt><dd>${e(display(value))}</dd>`).join('')}</dl>`; }
  function tests(record) {
    const test=record.tests;
    const logs=Array.isArray(record.reportLogs)?record.reportLogs:[];
    state.log=Math.min(state.log,Math.max(0,logs.length-1));
    const log=logs[state.log];
    const counters=test&&typeof test==='object'?`<div class="ff-test-summary">${[['started','Started'],['passed','Passed'],['failed','Failed'],['skipped','Skipped']].map(([key,label])=>`<span><strong>${e(number(test[key])??'—')}</strong>${label}</span>`).join('')}</div>`:'<p class="ff-evidence-missing">No test counters retained.</p>';
    return `${counters}${kv([['Reporter',record.reporterComplete===true?'Complete':record.reporterComplete===false?'Incomplete':'Not recorded'],['Runner status',record.runner?.status],['Cleanup',record.runner?.cleanup],['Exit code',record.exitCode]])}<div class="ff-captures"><h3>Captured output</h3>${logs.length>1?`<label class="ff-sr" for="ff-log-select">Capture stream</label><select id="ff-log-select" data-action="log" data-focus-key="log-select">${logs.map((item,index)=>`<option value="${index}" ${state.log===index?'selected':''}>${e(item.name)}</option>`).join('')}</select>`:''}${log?`${logs.length===1?`<p class="ff-log-name">${e(log.name)}</p>`:''}${log.unavailable?`<p class="ff-evidence-missing">${e(log.unavailable)}</p>`:`<pre class="ff-log">${e(log.text||'(empty capture)')}</pre>`}${log.truncated?'<p class="ff-bytehint">Capture preview is truncated. The full bytes remain in the run directory.</p>':''}`:'<p class="ff-evidence-missing">No capture preview is available.</p>'}</div>`;
  }
  function metadata(record) {
    const context=sourceContext(record);
    const safe=Object.fromEntries(Object.entries(record).filter(([key])=>key!=='reportLogs'));
    const raw=JSON.stringify(safe,null,2);
    const cap=256*1024;
    return `${kv([['Mutant ID',record.mutantId],['Family',record.family],['File',record.file],...(context?[['Line',context.line],['Column (UTF-16)',context.column],['End line',context.endLine],['Source SHA-256',context.sourceHash]]:[]),['Operator',record.operatorId],['Outcome',record.outcome],['Reason',record.reason],['Validation',typeof record.validation==='string'?record.validation:record.validation?.validation],['Compile checked',record.compileChecked===true?'Yes':record.compileChecked===false?'No':'Not recorded'],['Offset (UTF-8)',record.edit?.offsetBytes],['Replaced bytes',record.edit?.lengthBytes]])}<details class="ff-raw-details" data-raw-details ${state.rawDetails?'open':''}><summary>Retained result JSON</summary><pre class="ff-log">${e(raw.slice(0,cap))}</pre>${raw.length>cap?'<p class="ff-bytehint">Metadata preview is truncated. The complete result remains in the original run directory.</p>':''}</details>`;
  }
  function runMetadata(configuration) {
    const timeout=configuration.timeouts||{};
    const bool=value=>value===true?'Confirmed':value===false?'Not confirmed':'Not recorded';
    return kv([['Run ID',data.run.id],['Status',data.run.status],['Project',configuration.project],['Workers',configuration.workers],['Test timeout',timeout.test],['Command timeout',timeout.command],['Dart SDK',configuration.dart_sdk],['Flutter SDK',configuration.flutter_sdk],['Source unchanged',bool(data.run.originalProjectFilesUnchanged)],['Snapshot cleaned',bool(data.run.temporarySnapshotCleaned)],['Cleanup deferred',data.run.workspaceCleanupDeferred===true?'Yes':data.run.workspaceCleanupDeferred===false?'No':'Not recorded']]);
  }
  function selectedLocation() {
    const record=state.selected===null?null:activeRows().records[state.selected];
    const context=record&&sourceContext(record);
    return context?{key:JSON.stringify([String(activeFamily().id),String(record.mutantId)]),location:`${filename(record)}:${context.line}:${context.column}`}:null;
  }
  function sourceActions(record,context) {
    if (!context) return '';
    const key=JSON.stringify([String(activeFamily().id),String(record.mutantId)]);
    const notice=copyState.notice?.key===key?copyState.notice:null;
    const manual=copyState.manual?.key===key?copyState.manual:null;
    return `<div class="ff-source-actions" role="group" aria-label="Source actions">${button('copy-location','',copyState.pending===key?'Copying…':'Copy location','ff-page',copyState.pending?'disabled':'','copy-location')}</div>${notice?`<p class="ff-action-status" role="status">${e(notice.text)}</p>`:''}${manual?`<div class="ff-copy-fallback"><label for="ff-copy-location">Location — select and copy</label><input id="ff-copy-location" data-focus-key="copy-location-value" value="${e(manual.location)}" readonly aria-describedby="ff-copy-help" spellcheck="false"><p id="ff-copy-help">Use your device’s Copy action, or press Ctrl/Cmd+C.</p></div>`:''}`;
  }
  function refreshCopy(focusKey) {
    const focused=document.activeElement;
    const savedFocus=focused&&root.contains(focused)?focused.dataset?.focusKey:null;
    const selection=focused&&typeof focused.selectionStart==='number'?[focused.selectionStart,focused.selectionEnd]:null;
    const key=focusKey||savedFocus;
    render(key);
    if (selection&&!focusKey&&key) {
      const replacement=[...root.querySelectorAll('[data-focus-key]')].find(el=>el.dataset.focusKey===key);
      if (replacement&&typeof replacement.setSelectionRange==='function') replacement.setSelectionRange(...selection);
    }
  }
  function showManualLocation(target) {
    copyState.manual={key:target.key,location:target.location};
    copyState.notice={key:target.key,text:'Select the location below and copy it.'};
    const current=selectedLocation()?.key===target.key;
    refreshCopy(current?'copy-location-value':undefined);
    if (current) root.querySelector('#ff-copy-location')?.select();
  }
  async function copyLocation() {
    const target=selectedLocation();
    if (!target||copyState.pending) return;
    if (!globalThis.isSecureContext || typeof navigator==='undefined' || typeof navigator.clipboard?.writeText!=='function') { showManualLocation(target); return; }
    copyState.pending=target.key;
    copyState.notice=null;
    refreshCopy();
    try {
      await navigator.clipboard.writeText(target.location);
      copyState.manual=null;
      copyState.notice={key:target.key,text:`Copied ${target.location}.`};
    } catch (_) {
      copyState.pending=null;
      showManualLocation(target);
      return;
    }
    copyState.pending=null;
    refreshCopy();
  }
  function change(action,value) {
    switch(action) {
      case 'family': if(!families[Number(value)]?.enabled)return;state.family=Number(value);state.file=null;state.selected=null;state.page=0;state.filePage=0;state.log=0;resetFilter();break;
      case 'file': state.file=value==='all'?null:activeRows().files[Number(value)]??null;state.selected=null;state.page=0;state.log=0;break;
      case 'file-select': state.file=value||null;state.selected=null;state.page=0;state.log=0;break;
      case 'filter': if(!['all','SURVIVED','KILLED','excluded'].includes(value))return;state.filter=value;state.selected=null;state.page=0;state.log=0;break;
      case 'mutant': state.selected=Number(value);state.log=0;state.rawDetails=false;break;
      case 'tab': if(!['change','tests','metadata'].includes(value))return;state.tab=value;break;
      case 'page': state.page=Math.max(0,Number(value)||0);state.selected=null;state.log=0;break;
      case 'file-page': state.filePage=Math.max(0,Number(value)||0);break;
      case 'log': state.log=Math.max(0,Number(value)||0);break;
      default:return;
    }
  }
  root.addEventListener('click',event=>{
    const control=event.target.closest('button[data-action]');
    if(!control||!root.contains(control)||control.disabled)return;
    if(control.dataset.action==='copy-location'){void copyLocation();return;}
    change(control.dataset.action,control.dataset.value);
    render(control.dataset.focusKey);
  });
  root.addEventListener('change',event=>{
    const control=event.target.closest('select[data-action]');
    if(!control)return;
    change(control.dataset.action,control.value);
    render(control.dataset.focusKey);
  });
  root.addEventListener('keydown',event=>{
    if(!['ArrowLeft','ArrowRight','Home','End'].includes(event.key))return;
    const group=event.target.closest('[role="tablist"]');
    if(!group)return;
    const tabs=[...group.querySelectorAll('[role="tab"]')].filter(tab=>!tab.disabled);
    const index=tabs.indexOf(document.activeElement);
    if(index<0)return;
    event.preventDefault();
    const target=event.key==='Home'?0:event.key==='End'?tabs.length-1:(index+(event.key==='ArrowRight'?1:-1)+tabs.length)%tabs.length;
    tabs[target].click();
  });
  root.addEventListener('toggle',event=>{
    if(event.target.matches('[data-run-details]'))state.runDetails=event.target.open;
    if(event.target.matches('[data-raw-details]'))state.rawDetails=event.target.open;
  },true);
  render();
})();
