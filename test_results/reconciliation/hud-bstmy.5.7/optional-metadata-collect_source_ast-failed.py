from pathlib import Path
import subprocess,json,re,hashlib,gzip,time,os,sys
from tree_sitter import Language,Parser
import tree_sitter_rust
start=time.time();parser=Parser(Language(tree_sitter_rust.language()));base=Path('test_results/reconciliation/hud-bstmy.5.7');head=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()
tracked=[p for p in subprocess.check_output(['git','ls-files','-z'],text=True).split('\0') if p.endswith('.rs')]
interests=set('WorkCounts take_work_counts latest_work widget_rasterized raster_counts raster_count rasterized_last_sync rasterize_and_upload sync_widget_textures start_transition resolve_animated_params WidgetAnimationState WidgetTextureEntry animation current_params transition_ms published_at_wall_us publish_to_widget publish_to_widget_for_lease set_widget_param_local remove_texture has_active_transition next_animation_deadline has_inflight_animation render_frame render_frame_headless FrameTelemetry ChromeRenderer ChromeLayout ChromeDrawCmd SafeModeController MuteToggle classify_safe_mode_input strip_chrome_from_topology render_frame_with_chrome InvalidatonClosure InvalidationClosure ResourceMonitor'.split())
refs={k:[] for k in interests};decls=[];tests=[];paths=[];edges=[]
def txt(n): return n.text.decode(errors='replace') if n else None
def location(path,n):return {'path':path,'line':n.start_point.row+1,'end_line':n.end_point.row+1}
def attrs_of(node):
 a=[];prev=node.prev_named_sibling
 while prev and prev.type in ['attribute_item','line_comment','block_comment']:
  if prev.type=='attribute_item':a.insert(0,txt(prev))
  prev=prev.prev_named_sibling
 return a
for path in tracked:
 b=Path(path).read_bytes();tree=parser.parse(b);r='bench' if '/benches/' in path else 'existing-test-or-fixture' if '/tests/' in path or path.startswith('tests/') else 'production'
 pp=path.split('/');crate=pp[1] if pp[0]=='crates' else 'tze_hud_app' if pp[0]=='app' else pp[1] if pp[0]=='examples' else 'integration'
 tail=path.split('/src/',1)[-1] if '/src/' in path else Path(path).name;mod=tail.removesuffix('.rs').replace('/','::');mod=re.sub(r'(?:::)?(?:mod|lib|main)$','',mod).strip(':');prefix=[crate]+([mod] if mod else [])
 paths.append({'path':path,'sha256':hashlib.sha256(b).hexdigest(),'git_blob':subprocess.check_output(['git','rev-parse','HEAD:'+path],text=True).strip(),'role':r,'AST_has_error':tree.root_node.has_error})
 def walk(node,mods,owner,cfg,role):
  attrs=attrs_of(node);owncfg=[a for a in attrs if a.startswith('#[cfg')];allcfg=cfg+owncfg;localrole='existing-test-or-fixture' if role=='existing-test-or-fixture' or (any(re.search(r'\btest\b',q) for q in owncfg) and not any('not(test)' in q for q in owncfg)) else role
  if node.type=='mod_item':
   name=txt(node.child_by_field_name('name'));body=node.child_by_field_name('body')
   if not body:edges.append({**location(path,node),'module':name,'attributes':attrs,'cfg':allcfg,'source_module':'::'.join(mods)})
   mods=mods+([name] if name else [])
  elif node.type=='impl_item': owner=txt(node.child_by_field_name('type'))
  elif node.type in ['function_item','struct_item','enum_item','trait_item','type_item','const_item','static_item','macro_definition']:
   name=txt(node.child_by_field_name('name'));qual='::'.join(mods+([owner] if owner else [])+([name] if name else []));visibility=next((txt(c) for c in node.named_children if c.type=='visibility_modifier'),'private')
   row={**location(path,node),'kind':node.type,'name':name,'qualified_source_owner':qual,'declared_impl_owner':owner,'module':'::'.join(mods),'visibility':visibility,'cfg':allcfg,'attributes':attrs,'role':localrole,'source_sha256':hashlib.sha256(node.text).hexdigest()}
   if node.type=='struct_item':
    row['fields']=[{'name':txt(c.child_by_field_name('name')),'type':txt(c.child_by_field_name('type')),'line':c.start_point.row+1} for body in node.named_children if body.type=='field_declaration_list' for c in body.named_children if c.type=='field_declaration']
   decls.append(row)
   if node.type=='function_item' and any(re.search(r'#\[(?:test\]|(?:\w+::)*test(?:\(|\]))',a) for a in attrs):
    body=node.child_by_field_name('body'); tests.append({**row,'body_sha256':hashlib.sha256(body.text).hexdigest() if body else None,'assertion_lines':[n+1 for n,line in enumerate(b.decode(errors='replace').splitlines()) if node.start_point.row<=n<=node.end_point.row and re.search(r'assert(?:_eq|_ne)?!|assert_p99_under|\.expect\(',line)]});localrole='existing-test-or-fixture'
  if node.type in ['identifier','field_identifier','type_identifier']:
   name=txt(node)
   if name in refs:refs[name].append({**location(path,node),'role':localrole,'kind':node.type,'impl_owner':owner,'module':'::'.join(mods),'cfg':allcfg,'source_line':b.decode(errors='replace').splitlines()[node.start_point.row].strip()})
  for child in node.named_children:walk(child,mods,owner,allcfg,localrole)
 walk(tree.root_node,prefix,None,[],r)
artifact={'audited_source':head,'native_pid':os.getpid(),'native_cwd':str(Path.cwd()),'parser':'tree-sitter Rust cached uv offline environment','parse_executed_here':True,'paths':paths,'declarations':decls,'tests':tests,'module_edges':edges,'interested_AST_token_references':refs,'limits':['AST tokens enumerate comments-excluded references, not receiver type resolution; manual qualified producer/consumer adjudication required.','Source module qualification of impls does not relocate the receiver type; concrete exported receiver paths are adjudicated separately.','Macros can synthesize functions not represented as ordinary function_item; documented72 definitions are separately checked.','cfg propagation within files recorded; cross-file/target/dependency-kind edges need manifests and module declarations.']}
raw=(json.dumps(artifact,ensure_ascii=False,indent=2)+'\n').encode();(base/'fresh-source-AST.json.gz').write_bytes(gzip.compress(raw,mtime=0));(base/'fresh-source-AST-summary.json').write_text(json.dumps({'source':head,'native_pid':os.getpid(),'cwd':str(Path.cwd()),'parse_executed_here':True,'rust_paths':len(paths),'AST_error_paths':[p['path'] for p in paths if p['AST_has_error']],'declarations':len(decls),'attributed_test_declarations':len(tests),'reference_counts':{k:len(v) for k,v in refs.items()},'elapsed_seconds':round(time.time()-start,3),'artifact_raw_sha256':hashlib.sha256(raw).hexdigest(),'artifact_gzip_sha256':hashlib.sha256((base/'fresh-source-AST.json.gz').read_bytes()).hexdigest(),'command':['uv','run','--offline','--no-project','--with','tree-sitter','--with','tree-sitter-rust','python','.handoff/hud-bstmy.5.7/collect_source_ast.py'],'script_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),'limits':artifact['limits']},indent=2)+'\n')
print(json.dumps({'source':head,'PID':os.getpid(),'rust_paths':len(paths),'AST_error_paths':[p['path'] for p in paths if p['AST_has_error']],'declarations':len(decls),'tests':len(tests),'elapsed_seconds':round(time.time()-start,3),'sha256':hashlib.sha256(raw).hexdigest()}))
