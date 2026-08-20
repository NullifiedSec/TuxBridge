use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct StructuredDiagnostic {
    pub severity: String,
    pub tool: String,
    pub path: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub code: Option<String>,
    pub message: String,
}

pub fn parse_command_diagnostics(argv:&[String],stdout:&str,stderr:&str)->Vec<StructuredDiagnostic>{
    let tool=argv.first().map(String::as_str).unwrap_or("command");
    let text=if stderr.trim().is_empty(){stdout}else{stderr};
    match tool{
        "cargo"|"rustc"=>parse_rust(tool,text),
        "tsc"|"vue-tsc"|"eslint"|"biome"=>parse_colon_or_ts(tool,text),
        "go"=>parse_colon(tool,text),
        "python"|"python3"|"pytest"=>parse_python(tool,text),
        "php"|"phpstan"|"psalm"=>parse_colon(tool,text),
        _=>parse_colon(tool,text),
    }
}

fn parse_rust(tool:&str,text:&str)->Vec<StructuredDiagnostic>{
    let mut out=Vec::new();let mut pending:Option<(String,String,Option<String>)>=None;
    for line in text.lines(){let t=line.trim();
        if let Some(rest)=t.strip_prefix("error"){let (code,msg)=if let Some(r)=rest.strip_prefix('['){if let Some(end)=r.find(']'){(Some(r[..end].to_owned()),r[end+1..].trim_start_matches(':').trim().to_owned())}else{(None,t.into())}}else{(None,rest.trim_start_matches(':').trim().to_owned())};pending=Some(("error".into(),if msg.is_empty(){"compiler error".into()}else{msg},code));continue;}
        if t.starts_with("warning:"){pending=Some(("warning".into(),t.trim_start_matches("warning:").trim().into(),None));continue;}
        if let Some(loc)=t.strip_prefix("-->"){if let Some((path,line_no,col))=parse_location(loc.trim()){let (severity,message,code)=pending.take().unwrap_or_else(||("error".into(),"compiler diagnostic".into(),None));out.push(StructuredDiagnostic{severity,tool:tool.into(),path:Some(path),line:Some(line_no),column:col,code,message});}}
        if out.len()>=500{break;}
    }out
}

fn parse_colon_or_ts(tool:&str,text:&str)->Vec<StructuredDiagnostic>{
    let mut out=Vec::new();for line in text.lines(){let t=line.trim();
        if let Some(open)=t.find('('){if let Some(close)=t[open+1..].find(')'){let close=open+1+close;let coords=&t[open+1..close];let mut p=coords.split(',');if let (Ok(ln),Ok(col))=(p.next().unwrap_or("").parse(),p.next().unwrap_or("").parse()){let rest=t[close+1..].trim_start_matches(':').trim();let (severity,code,message)=classify_message(rest);out.push(StructuredDiagnostic{severity,tool:tool.into(),path:Some(t[..open].into()),line:Some(ln),column:Some(col),code,message});continue;}}}
        if let Some((path,ln,col,msg))=parse_colon_line(t){let (severity,code,message)=classify_message(msg);out.push(StructuredDiagnostic{severity,tool:tool.into(),path:Some(path.into()),line:Some(ln),column:col,code,message});}
        if out.len()>=500{break;}
    }out
}

fn parse_colon(tool:&str,text:&str)->Vec<StructuredDiagnostic>{let mut out=Vec::new();for line in text.lines(){if let Some((path,ln,col,msg))=parse_colon_line(line.trim()){let (severity,code,message)=classify_message(msg);out.push(StructuredDiagnostic{severity,tool:tool.into(),path:Some(path.into()),line:Some(ln),column:col,code,message});if out.len()>=500{break;}}}out}

fn parse_python(tool:&str,text:&str)->Vec<StructuredDiagnostic>{let mut out=Vec::new();let mut last:Option<(String,usize)>=None;for line in text.lines(){let t=line.trim();if let Some(rest)=t.strip_prefix("File \""){if let Some(end)=rest.find("\""){let path=&rest[..end];let tail=&rest[end+1..];if let Some(pos)=tail.find("line "){if let Ok(ln)=tail[pos+5..].split(|c:char|!c.is_ascii_digit()).next().unwrap_or("").parse(){last=Some((path.into(),ln));}}}}else if (t.contains("Error")||t.starts_with("E   "))&&!t.is_empty(){let(path,ln)=last.clone().map(|v|(Some(v.0),Some(v.1))).unwrap_or((None,None));out.push(StructuredDiagnostic{severity:"error".into(),tool:tool.into(),path,line:ln,column:None,code:None,message:t.trim_start_matches("E   ").into()});if out.len()>=500{break;}}}out}

fn parse_location(s:&str)->Option<(String,usize,Option<usize>)>{let mut parts=s.rsplitn(3,':');let col=parts.next()?.parse::<usize>().ok();let line=parts.next()?.parse::<usize>().ok()?;let path=parts.next()?.to_owned();Some((path,line,col))}
fn parse_colon_line(s:&str)->Option<(&str,usize,Option<usize>,&str)>{let mut it=s.match_indices(':');let(a,_)=it.next()?;let(b,_)=it.next()?;let path=&s[..a];let line=s[a+1..b].parse().ok()?;let rest=&s[b+1..];if let Some(c_rel)=rest.find(':'){if let Ok(col)=rest[..c_rel].parse(){return Some((path,line,Some(col),rest[c_rel+1..].trim()));}}Some((path,line,None,rest.trim()))}
fn classify_message(s:&str)->(String,Option<String>,String){let lower=s.to_ascii_lowercase();let severity=if lower.contains("warning"){"warning"}else if lower.contains("note")||lower.contains("info"){"info"}else{"error"}.into();let mut code=None;for token in s.split_whitespace(){let clean=token.trim_matches(|c:char|c=='['||c==']'||c==':'||c==',');if clean.len()>1&&(clean.starts_with('E')||clean.starts_with("TS"))&&clean[1..].chars().any(|c|c.is_ascii_digit()){code=Some(clean.into());break;}}(severity,code,s.into())}

#[cfg(test)]mod tests{use super::*;#[test]fn parses_rust_location(){let d=parse_command_diagnostics(&["cargo".into(),"check".into()],"","error[E0308]: mismatched types\n --> src/main.rs:12:5\n");assert_eq!(d[0].path.as_deref(),Some("src/main.rs"));assert_eq!(d[0].line,Some(12));}#[test]fn parses_ts(){let d=parse_command_diagnostics(&["tsc".into()],"src/a.ts(4,9): error TS2322: bad"," ");assert_eq!(d[0].column,Some(9));}}
