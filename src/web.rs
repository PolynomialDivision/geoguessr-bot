//! HTTP server for interactive map-based guess submission.
//!
//! Each DM participant receives a personal token URL posted in the game room.
//! Opening the URL shows a Leaflet world map.  Clicking places a draggable
//! pin; pressing Submit records the guess.  Tokens expire at round end.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
};
use matrix_sdk::{
    Client,
    ruma::{OwnedEventId, OwnedRoomId, OwnedUserId},
};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, sync::Mutex};
use tracing::{info, warn};

use crate::game::{ActiveGame, ActiveGameMode, FreeGuess};

// ── Token + session store ─────────────────────────────────────────────────────

/// One record per player per guess.
#[derive(Clone, Debug)]
pub struct GuessToken {
    pub user_id:   OwnedUserId,
    pub round_id:  i64,
    pub guess_num: u32,
    pub lang:      String,
}

/// Per-guess session shared across all tokens for the same (round_id, guess_num).
#[derive(Clone, Debug)]
pub struct GuessSession {
    /// Event ID of the links message in the game room — edited on each submission.
    pub links_event_id: OwnedEventId,
    /// Ordered participant list (controls display order in status line).
    pub participants:   Vec<OwnedUserId>,
}

#[derive(Default)]
pub struct TokenStore {
    pub tokens:   HashMap<String, GuessToken>,
    pub sessions: HashMap<(i64, u32), GuessSession>,
}

pub type SharedTokenStore = Arc<Mutex<TokenStore>>;

pub fn new_token_store() -> SharedTokenStore {
    Arc::new(Mutex::new(TokenStore::default()))
}

// ── Axum shared state ─────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WebState {
    pub store:       SharedTokenStore,
    pub active_game: Arc<Mutex<Option<ActiveGame>>>,
    pub client:      Client,
    pub room_id:     OwnedRoomId,
    /// 0 = unlimited updates; >0 = only the first submission counts.
    pub max_guesses: u32,
    /// Public base URL (no trailing slash).
    pub public_url:  String,
}

// ── Server entry point ────────────────────────────────────────────────────────

pub async fn run(bind_addr: String, state: WebState) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/g/:token",        get(serve_map))
        .route("/g/:token/submit", post(submit_guess))
        .with_state(state);

    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Geo-guess web server listening on {bind_addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ── Map page ──────────────────────────────────────────────────────────────────

async fn serve_map(
    Path(token): Path<String>,
    State(ws):   State<WebState>,
) -> Response {
    let info = {
        let store = ws.store.lock().await;
        store.tokens.get(&token).cloned()
    };
    let Some(tok) = info else {
        return (StatusCode::NOT_FOUND, Html(expired_html())).into_response();
    };
    let one_shot = ws.max_guesses > 0;
    Html(map_html(&token, tok.user_id.localpart(), one_shot, &tok.lang)).into_response()
}

// ── Guess submission ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SubmitBody {
    lat: f64,
    lon: f64,
}

#[derive(Serialize)]
struct SubmitResponse {
    geocoded: Option<String>,
}

async fn submit_guess(
    Path(token): Path<String>,
    State(ws):   State<WebState>,
    Json(body):  Json<SubmitBody>,
) -> Response {
    if !is_valid_coords(body.lat, body.lon) {
        return (StatusCode::BAD_REQUEST, "Invalid coordinates").into_response();
    }

    let tok = {
        let store = ws.store.lock().await;
        match store.tokens.get(&token).cloned() {
            Some(t) => t,
            None    => return (StatusCode::NOT_FOUND, "Link expired or invalid").into_response(),
        }
    };

    let already_submitted = {
        let ag = ws.active_game.lock().await;
        ag.as_ref().map_or(false, |g| {
            let ActiveGameMode::FreeGuess { ref guesses, .. } = g.mode;
            guesses.contains_key(tok.user_id.as_str())
        })
    };

    // One-shot: acknowledge silently but don't overwrite the original guess.
    if already_submitted && ws.max_guesses > 0 {
        let geocoded = crate::geocode::reverse_geocode(body.lat, body.lon, &tok.lang).await;
        return (StatusCode::OK, Json(SubmitResponse { geocoded })).into_response();
    }

    let accepted = {
        let mut ag = ws.active_game.lock().await;
        ag.as_mut().map_or(false, |g| {
            g.record_free_guess(
                tok.user_id.as_str().to_owned(),
                FreeGuess {
                    text:         format!("{:.4},{:.4}", body.lat, body.lon),
                    lat:          body.lat,
                    lon:          body.lon,
                    submitted_at: chrono::Utc::now(),
                },
                ws.max_guesses,
            )
        })
    };

    info!(
        "Web guess: {} → ({:.4}, {:.4}) accepted={}",
        tok.user_id, body.lat, body.lon, accepted
    );

    if accepted {
        update_links_message(&ws, tok.round_id, tok.guess_num).await;
    }

    let geocoded = crate::geocode::reverse_geocode(body.lat, body.lon, &tok.lang).await;
    (StatusCode::OK, Json(SubmitResponse { geocoded })).into_response()
}

// ── Links message refresh ─────────────────────────────────────────────────────

/// Edit the "🗺️ Alice · Bob · Carol" message in the game room to reflect
/// who has submitted (✅) vs who is still pending (clickable map link).
pub async fn update_links_message(ws: &WebState, round_id: i64, guess_num: u32) {
    let (session, token_by_user) = {
        let store = ws.store.lock().await;
        let session = match store.sessions.get(&(round_id, guess_num)).cloned() {
            Some(s) => s,
            None    => return,
        };
        let token_by_user: HashMap<String, String> = store.tokens.iter()
            .filter(|(_, t)| t.round_id == round_id && t.guess_num == guess_num)
            .map(|(tok, t)| (t.user_id.to_string(), tok.clone()))
            .collect();
        (session, token_by_user)
    };

    let submitted: HashSet<String> = {
        let ag = ws.active_game.lock().await;
        ag.as_ref().map_or_else(HashSet::new, |g| {
            let ActiveGameMode::FreeGuess { ref guesses, .. } = g.mode;
            guesses.keys().cloned().collect()
        })
    };

    let line = session.participants.iter().map(|uid| {
        if submitted.contains(uid.as_str()) {
            format!("✅ {}", uid.localpart())
        } else if let Some(tok) = token_by_user.get(uid.as_str()) {
            format!("[🗺️ {}]({}/g/{})", uid.localpart(), ws.public_url, tok)
        } else {
            format!("⏳ {}", uid.localpart())
        }
    }).collect::<Vec<_>>().join("  ·  ");

    let Some(room) = ws.client.get_room(&ws.room_id) else { return };
    use matrix_sdk::ruma::events::room::message::ReplacementMetadata;
    let edit = crate::format::mentionify(&line)
        .make_replacement(ReplacementMetadata::new(session.links_event_id.clone(), None));
    if let Err(e) = room.send(edit).await {
        warn!("Failed to update links message: {e}");
    }
}

// ── Token generation ──────────────────────────────────────────────────────────

pub fn generate_token() -> String {
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(24)
        .map(char::from)
        .collect()
}

// ── Coordinate validation ─────────────────────────────────────────────────────

pub fn is_valid_coords(lat: f64, lon: f64) -> bool {
    lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0
}

// ── HTML ──────────────────────────────────────────────────────────────────────

fn map_html(token: &str, display_name: &str, one_shot: bool, lang: &str) -> String {
    // Use placeholder replacement so Leaflet's {z}/{x}/{y}/{s} tokens and
    // JavaScript object literals don't need escaping in a format! string.
    let token_json    = serde_json::to_string(token).unwrap_or_else(|_| "\"\"".to_owned());
    let name_json     = serde_json::to_string(display_name).unwrap_or_else(|_| "\"\"".to_owned());
    let one_shot_js   = if one_shot { "true" } else { "false" };
    let lang_json     = serde_json::to_string(lang).unwrap_or_else(|_| "\"en\"".to_owned());

    LEAFLET_MAP_TEMPLATE
        .replace("__TOKEN__", &token_json)
        .replace("__NAME__", &name_json)
        .replace("__ONE_SHOT__", one_shot_js)
        .replace("__LANG__", &lang_json)
}

static LEAFLET_MAP_TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>GeoGuessr – Place your guess</title>
<link rel="stylesheet" href="https://unpkg.com/maplibre-gl@4/dist/maplibre-gl.css">
<style>
*,*::before,*::after{box-sizing:border-box;margin:0;padding:0}
html,body{height:100%;background:#1a1a2e;font-family:system-ui,sans-serif}
#map{height:calc(100% - 60px)}
#bar{
  position:fixed;bottom:0;left:0;right:0;height:60px;
  display:flex;align-items:center;gap:12px;padding:0 16px;
  background:#16213e;color:#e0e0f0;border-top:1px solid #2a2a4a;
  z-index:1000;
}
#hint{flex:1;font-size:14px;color:#99aacc}
#name{font-size:13px;color:#556;white-space:nowrap}
#btn{
  padding:10px 22px;border:none;border-radius:8px;font-size:15px;
  font-weight:600;cursor:pointer;white-space:nowrap;transition:background .15s;
}
#btn:disabled{background:#2a2a4a;color:#556;cursor:default}
#btn.ready{background:#2980b9;color:#fff}
#btn.ready:hover{background:#3498db}
#btn.done-update{background:#27ae60;color:#fff}
#btn.done-update:hover{background:#2ecc71}
#btn.done-locked{background:#1e3a2a;color:#5a8a6a;cursor:default}
</style>
</head>
<body>
<div id="map"></div>
<div id="bar">
  <span id="hint">Tap the map to place your pin</span>
  <button id="btn" disabled>Submit guess</button>
  <span id="name"></span>
</div>
<script src="https://unpkg.com/maplibre-gl@4/dist/maplibre-gl.js"></script>
<script>
(function(){
  var TOKEN    = __TOKEN__;
  var NAME     = __NAME__;
  var ONE_SHOT = __ONE_SHOT__;
  var LANG     = __LANG__;

  var T = {
    en:{tap:'Tap the map to place your pin',submit:'Submit guess',submitting:'Submitting…',locked_in:'✅ Locked in: ',locked_btn:'✅ Guess locked in',update:'✏️ Update guess',recorded:'✅ Guess recorded: ',net_err:'❌ Network error — try again',err:'Error — try again'},
    de:{tap:'Karte antippen, um Stecknadel zu setzen',submit:'Antwort absenden',submitting:'Wird gesendet…',locked_in:'✅ Festgelegt: ',locked_btn:'✅ Antwort festgelegt',update:'✏️ Antwort ändern',recorded:'✅ Antwort gespeichert: ',net_err:'❌ Netzwerkfehler — erneut versuchen',err:'Fehler — erneut versuchen'},
    fr:{tap:'Appuyez sur la carte pour placer votre repère',submit:'Soumettre',submitting:'Envoi…',locked_in:'✅ Verrouillé : ',locked_btn:'✅ Réponse verrouillée',update:'✏️ Modifier',recorded:'✅ Réponse enregistrée : ',net_err:'❌ Erreur réseau — réessayez',err:'Erreur — réessayez'},
    es:{tap:'Toca el mapa para colocar tu pin',submit:'Enviar respuesta',submitting:'Enviando…',locked_in:'✅ Confirmado: ',locked_btn:'✅ Respuesta confirmada',update:'✏️ Actualizar respuesta',recorded:'✅ Respuesta guardada: ',net_err:'❌ Error de red — inténtalo de nuevo',err:'Error — inténtalo de nuevo'},
    ru:{tap:'Нажмите на карту, чтобы поставить метку',submit:'Отправить ответ',submitting:'Отправка…',locked_in:'✅ Зафиксировано: ',locked_btn:'✅ Ответ зафиксирован',update:'✏️ Изменить ответ',recorded:'✅ Ответ записан: ',net_err:'❌ Ошибка сети — попробуйте снова',err:'Ошибка — попробуйте снова'},
    it:{tap:'Tocca la mappa per posizionare il pin',submit:'Invia risposta',submitting:'Invio…',locked_in:'✅ Bloccato: ',locked_btn:'✅ Risposta bloccata',update:'✏️ Aggiorna risposta',recorded:'✅ Risposta registrata: ',net_err:'❌ Errore di rete — riprova',err:'Errore — riprova'},
    pl:{tap:'Dotknij mapę, aby umieścić pinezkę',submit:'Prześlij odpowiedź',submitting:'Wysyłanie…',locked_in:'✅ Zablokowano: ',locked_btn:'✅ Odpowiedź zablokowana',update:'✏️ Zaktualizuj odpowiedź',recorded:'✅ Odpowiedź zapisana: ',net_err:'❌ Błąd sieci — spróbuj ponownie',err:'Błąd — spróbuj ponownie'},
    nl:{tap:'Tik op de kaart om je pin te plaatsen',submit:'Antwoord verzenden',submitting:'Verzenden…',locked_in:'✅ Vergrendeld: ',locked_btn:'✅ Antwoord vergrendeld',update:'✏️ Antwoord bijwerken',recorded:'✅ Antwoord opgeslagen: ',net_err:'❌ Netwerkfout — probeer opnieuw',err:'Fout — probeer opnieuw'},
    pt:{tap:'Toque no mapa para colocar seu marcador',submit:'Enviar resposta',submitting:'Enviando…',locked_in:'✅ Confirmado: ',locked_btn:'✅ Resposta confirmada',update:'✏️ Atualizar resposta',recorded:'✅ Resposta registada: ',net_err:'❌ Erro de rede — tente novamente',err:'Erro — tente novamente'},
    uk:{tap:'Натисніть на карту, щоб поставити мітку',submit:'Надіслати відповідь',submitting:'Надсилання…',locked_in:'✅ Зафіксовано: ',locked_btn:'✅ Відповідь зафіксована',update:'✏️ Оновити відповідь',recorded:'✅ Відповідь збережено: ',net_err:'❌ Помилка мережі — спробуйте ще раз',err:'Помилка — спробуйте ще раз'},
    ja:{tap:'地図をタップしてピンを置く',submit:'回答を送信',submitting:'送信中…',locked_in:'✅ 確定: ',locked_btn:'✅ 回答を確定',update:'✏️ 回答を更新',recorded:'✅ 回答を記録: ',net_err:'❌ ネットワークエラー — 再試行',err:'エラー — 再試行'},
    zh:{tap:'点击地图放置图钉',submit:'提交答案',submitting:'提交中…',locked_in:'✅ 已确认: ',locked_btn:'✅ 答案已确认',update:'✏️ 更新答案',recorded:'✅ 答案已记录: ',net_err:'❌ 网络错误 — 请重试',err:'错误 — 请重试'},
    ar:{tap:'اضغط على الخريطة لوضع الدبوس',submit:'إرسال الإجابة',submitting:'جارٍ الإرسال…',locked_in:'✅ تم التأكيد: ',locked_btn:'✅ تم تأكيد الإجابة',update:'✏️ تحديث الإجابة',recorded:'✅ تم تسجيل الإجابة: ',net_err:'❌ خطأ في الشبكة — حاول مرة أخرى',err:'خطأ — حاول مرة أخرى'},
    tr:{tap:'Pini yerleştirmek için haritaya dokun',submit:'Tahmin gönder',submitting:'Gönderiliyor…',locked_in:'✅ Kilitlendi: ',locked_btn:'✅ Tahmin kilitlendi',update:'✏️ Tahmini güncelle',recorded:'✅ Tahmin kaydedildi: ',net_err:'❌ Ağ hatası — tekrar dene',err:'Hata — tekrar dene'},
    sv:{tap:'Tryck på kartan för att placera din nål',submit:'Skicka svar',submitting:'Skickar…',locked_in:'✅ Låst: ',locked_btn:'✅ Svar låst',update:'✏️ Uppdatera svar',recorded:'✅ Svar registrerat: ',net_err:'❌ Nätverksfel — försök igen',err:'Fel — försök igen'},
    fi:{tap:'Napauta karttaa asettaaksesi nuppineulan',submit:'Lähetä arvaus',submitting:'Lähetetään…',locked_in:'✅ Lukittu: ',locked_btn:'✅ Arvaus lukittu',update:'✏️ Päivitä arvaus',recorded:'✅ Arvaus tallennettu: ',net_err:'❌ Verkkovirhe — yritä uudelleen',err:'Virhe — yritä uudelleen'},
    da:{tap:'Tryk på kortet for at placere din nål',submit:'Send gæt',submitting:'Sender…',locked_in:'✅ Låst: ',locked_btn:'✅ Gæt låst',update:'✏️ Opdater gæt',recorded:'✅ Gæt registreret: ',net_err:'❌ Netværksfejl — prøv igen',err:'Fejl — prøv igen'},
    cs:{tap:'Klepněte na mapu a umístěte špendlík',submit:'Odeslat odpověď',submitting:'Odesílám…',locked_in:'✅ Uzamčeno: ',locked_btn:'✅ Odpověď uzamčena',update:'✏️ Aktualizovat odpověď',recorded:'✅ Odpověď zaznamenána: ',net_err:'❌ Chyba sítě — zkuste znovu',err:'Chyba — zkuste znovu'},
    hu:{tap:'Koppintson a térképre a gombostű elhelyezéséhez',submit:'Tipp elküldése',submitting:'Küldés…',locked_in:'✅ Rögzítve: ',locked_btn:'✅ Tipp rögzítve',update:'✏️ Tipp frissítése',recorded:'✅ Tipp rögzítve: ',net_err:'❌ Hálózati hiba — próbálja újra',err:'Hiba — próbálja újra'},
    ro:{tap:'Atinge harta pentru a plasa acul',submit:'Trimite răspuns',submitting:'Se trimite…',locked_in:'✅ Blocat: ',locked_btn:'✅ Răspuns blocat',update:'✏️ Actualizează răspuns',recorded:'✅ Răspuns înregistrat: ',net_err:'❌ Eroare de rețea — încearcă din nou',err:'Eroare — încearcă din nou'},
    el:{tap:'Πατήστε στον χάρτη για να τοποθετήσετε την καρφίτσα',submit:'Αποστολή απάντησης',submitting:'Αποστολή…',locked_in:'✅ Κλειδωμένο: ',locked_btn:'✅ Απάντηση κλειδωμένη',update:'✏️ Ενημέρωση απάντησης',recorded:'✅ Απάντηση καταγράφηκε: ',net_err:'❌ Σφάλμα δικτύου — δοκιμάστε ξανά',err:'Σφάλμα — δοκιμάστε ξανά'},
    he:{tap:'גע במפה כדי למקם את הסיכה',submit:'שלח תשובה',submitting:'שולח…',locked_in:'✅ נעול: ',locked_btn:'✅ תשובה נעולה',update:'✏️ עדכן תשובה',recorded:'✅ תשובה נרשמה: ',net_err:'❌ שגיאת רשת — נסה שוב',err:'שגיאה — נסה שוב'},
    ko:{tap:'지도를 눌러 핀을 놓으세요',submit:'답안 제출',submitting:'제출 중…',locked_in:'✅ 확정됨: ',locked_btn:'✅ 답안 확정',update:'✏️ 답안 수정',recorded:'✅ 답안 기록됨: ',net_err:'❌ 네트워크 오류 — 다시 시도',err:'오류 — 다시 시도'},
    th:{tap:'แตะแผนที่เพื่อวางหมุด',submit:'ส่งคำตอบ',submitting:'กำลังส่ง…',locked_in:'✅ ล็อคแล้ว: ',locked_btn:'✅ ล็อคคำตอบแล้ว',update:'✏️ อัปเดตคำตอบ',recorded:'✅ บันทึกคำตอบแล้ว: ',net_err:'❌ เครือข่ายผิดพลาด — ลองอีกครั้ง',err:'ผิดพลาด — ลองอีกครั้ง'},
    vi:{tap:'Chạm vào bản đồ để đặt ghim',submit:'Gửi đoán',submitting:'Đang gửi…',locked_in:'✅ Đã xác nhận: ',locked_btn:'✅ Đã khóa đoán',update:'✏️ Cập nhật đoán',recorded:'✅ Đoán đã được ghi: ',net_err:'❌ Lỗi mạng — thử lại',err:'Lỗi — thử lại'},
    id:{tap:'Ketuk peta untuk meletakkan pin',submit:'Kirim tebakan',submitting:'Mengirim…',locked_in:'✅ Dikunci: ',locked_btn:'✅ Tebakan dikunci',update:'✏️ Perbarui tebakan',recorded:'✅ Tebakan dicatat: ',net_err:'❌ Kesalahan jaringan — coba lagi',err:'Kesalahan — coba lagi'}
  };

  function t(key) {
    var base = (LANG || 'en').split('-')[0];
    var row  = T[base] || T['en'];
    return row[key] || T['en'][key] || key;
  }

  if (NAME) document.getElementById('name').textContent = '👤 ' + NAME;

  var map = new maplibregl.Map({
    container: 'map',
    style:     'https://tiles.openfreemap.org/styles/liberty',
    center:    [0, 20],
    zoom:      2,
  });

  var lang = (LANG || 'en').split('-')[0];
  map.once('load', function() {
    map.getStyle().layers.forEach(function(layer) {
      if (layer.layout && layer.layout['text-field']) {
        map.setLayoutProperty(layer.id, 'text-field', [
          'coalesce',
          ['get', 'name:' + lang],
          ['get', 'name:en'],
          ['get', 'name'],
        ]);
      }
    });
  });

  var hint      = document.getElementById('hint');
  var btn       = document.getElementById('btn');
  var pin       = null;
  var pinLat    = null;
  var pinLon    = null;
  var submitted = false;

  hint.textContent = t('tap');
  btn.textContent  = t('submit');

  function handleClick(e) {
    if (submitted && ONE_SHOT) return;
    pinLat = e.lngLat.lat;
    pinLon = e.lngLat.lng;
    if (pin) {
      pin.setLngLat([pinLon, pinLat]);
    } else {
      pin = new maplibregl.Marker({draggable: true})
        .setLngLat([pinLon, pinLat])
        .addTo(map);
      pin.on('dragend', function() {
        if (submitted && ONE_SHOT) return;
        var ll = pin.getLngLat();
        pinLat = ll.lat;
        pinLon = ll.lng;
        hint.textContent = fmt(pinLat, pinLon);
        if (!submitted) setReady();
      });
    }
    hint.textContent = fmt(pinLat, pinLon);
    if (!submitted || !ONE_SHOT) setReady();
  }
  map.on('click', handleClick);

  function setReady() {
    btn.disabled = false;
    btn.className = 'ready';
    btn.textContent = submitted ? t('update') : t('submit');
  }

  btn.addEventListener('click', async function() {
    if (!pinLat || btn.disabled) return;
    btn.disabled = true;
    btn.textContent = t('submitting');
    try {
      var r = await fetch('/g/' + TOKEN + '/submit', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({lat: pinLat, lon: pinLon}),
      });
      if (r.ok) {
        var j = await r.json();
        var place = (j && j.geocoded) ? j.geocoded : fmt(pinLat, pinLon);
        submitted = true;
        if (ONE_SHOT) {
          hint.textContent = t('locked_in') + place;
          btn.textContent = t('locked_btn');
          btn.className = 'done-locked';
          btn.disabled = true;
          map.off('click', handleClick);
          if (pin) pin.setDraggable(false);
        } else {
          hint.textContent = t('recorded') + place;
          btn.textContent = t('update');
          btn.className = 'done-update';
          btn.disabled = false;
        }
      } else {
        var txt = await r.text();
        hint.textContent = '❌ ' + (txt || t('err'));
        setReady();
      }
    } catch(e) {
      hint.textContent = t('net_err');
      setReady();
    }
  });

  function fmt(lat, lon) { return lat.toFixed(3) + ', ' + lon.toFixed(3); }
})();
</script>
</body>
</html>"#;

fn expired_html() -> String {
    r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>Link expired</title>
<style>
body{background:#1a1a2e;color:#e0e0f0;font-family:system-ui,sans-serif;
  display:flex;align-items:center;justify-content:center;height:100vh;margin:0}
div{text-align:center}
h2{font-size:1.4rem;margin-bottom:12px}
p{color:#99aacc;font-size:0.95rem}
</style></head>
<body><div>
<h2>⏰ This link has expired</h2>
<p>The round has ended or the link is invalid.</p>
</div></body></html>"#.to_owned()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use matrix_sdk::ruma::OwnedUserId;
    use crate::game::{ActiveGame, ActiveGameMode};

    // ── Token generation ──────────────────────────────────────────────────────

    #[test]
    fn token_is_24_alphanumeric_chars() {
        let t = generate_token();
        assert_eq!(t.len(), 24, "token must be 24 chars");
        assert!(t.chars().all(|c| c.is_ascii_alphanumeric()), "token must be alphanumeric");
    }

    #[test]
    fn tokens_are_unique() {
        let tokens: Vec<String> = (0..200).map(|_| generate_token()).collect();
        let set: HashSet<&String> = tokens.iter().collect();
        assert_eq!(set.len(), tokens.len(), "all 200 generated tokens must be distinct");
    }

    // ── Token store ───────────────────────────────────────────────────────────

    #[test]
    fn token_store_add_and_retrieve() {
        let uid: OwnedUserId = "@alice:example.com".try_into().unwrap();
        let mut store = TokenStore::default();
        store.tokens.insert("tok1".to_owned(), GuessToken {
            user_id: uid.clone(), round_id: 1, guess_num: 1, lang: "en".to_owned(),
        });
        let found = store.tokens.get("tok1");
        assert!(found.is_some());
        assert_eq!(found.unwrap().user_id, uid);
        assert!(store.tokens.get("wrong_token").is_none());
    }

    #[test]
    fn token_store_clear_by_round_and_guess() {
        let uid: OwnedUserId = "@alice:example.com".try_into().unwrap();
        let mut store = TokenStore::default();
        store.tokens.insert("t_r1g1".to_owned(), GuessToken { user_id: uid.clone(), round_id: 1, guess_num: 1, lang: "en".to_owned() });
        store.tokens.insert("t_r1g2".to_owned(), GuessToken { user_id: uid.clone(), round_id: 1, guess_num: 2, lang: "en".to_owned() });
        store.tokens.insert("t_r2g1".to_owned(), GuessToken { user_id: uid.clone(), round_id: 2, guess_num: 1, lang: "en".to_owned() });

        // Simulate clearing round 1, guess 1.
        store.tokens.retain(|_, t| !(t.round_id == 1 && t.guess_num == 1));
        store.sessions.remove(&(1, 1));

        assert!(!store.tokens.contains_key("t_r1g1"), "cleared token must be gone");
        assert!(store.tokens.contains_key("t_r1g2"), "other guess token must survive");
        assert!(store.tokens.contains_key("t_r2g1"), "other round token must survive");
    }

    #[test]
    fn token_store_full_clear() {
        let uid: OwnedUserId = "@alice:example.com".try_into().unwrap();
        let mut store = TokenStore::default();
        for i in 0..5 {
            store.tokens.insert(format!("tok{i}"), GuessToken { user_id: uid.clone(), round_id: 1, guess_num: i, lang: "en".to_owned() });
        }
        store.tokens.clear();
        store.sessions.clear();
        assert!(store.tokens.is_empty());
        assert!(store.sessions.is_empty());
    }

    // ── Coordinate validation ─────────────────────────────────────────────────

    #[test]
    fn coord_validation_accepts_valid() {
        assert!(is_valid_coords(0.0,   0.0));
        assert!(is_valid_coords(90.0,  180.0));
        assert!(is_valid_coords(-90.0, -180.0));
        assert!(is_valid_coords(48.13, 11.58));
        assert!(is_valid_coords(-33.87, 151.21));
    }

    #[test]
    fn coord_validation_rejects_out_of_range() {
        assert!(!is_valid_coords(90.01,  0.0));
        assert!(!is_valid_coords(-90.01, 0.0));
        assert!(!is_valid_coords(0.0,    180.01));
        assert!(!is_valid_coords(0.0,   -180.01));
    }

    #[test]
    fn coord_validation_rejects_non_finite() {
        assert!(!is_valid_coords(f64::NAN,       0.0));
        assert!(!is_valid_coords(0.0,            f64::NAN));
        assert!(!is_valid_coords(f64::INFINITY,  0.0));
        assert!(!is_valid_coords(0.0,            f64::NEG_INFINITY));
    }

    // ── Guess recording ───────────────────────────────────────────────────────

    fn make_game() -> ActiveGame {
        ActiveGame {
            event_id: "$test:example.com".try_into().unwrap(),
            mode: ActiveGameMode::FreeGuess {
                guesses:    HashMap::new(),
                actual_lat: 48.13,
                actual_lon: 11.58,
            },
        }
    }

    fn make_guess(lat: f64, lon: f64) -> FreeGuess {
        FreeGuess { text: format!("{lat},{lon}"), lat, lon, submitted_at: chrono::Utc::now() }
    }

    fn guesses_of(ag: &ActiveGame) -> &HashMap<String, FreeGuess> {
        let ActiveGameMode::FreeGuess { ref guesses, .. } = ag.mode;
        guesses
    }

    #[test]
    fn submit_records_guess() {
        let mut ag = make_game();
        let uid = "@alice:example.com".to_owned();
        let accepted = ag.record_free_guess(uid.clone(), make_guess(10.0, 20.0), 0);
        assert!(accepted, "first submission should be accepted");
        let guesses = guesses_of(&ag);
        assert!(guesses.contains_key(&uid), "guess must be stored");
        assert_eq!(guesses[&uid].lat, 10.0);
    }

    #[test]
    fn one_shot_blocks_second_submission() {
        let mut ag = make_game();
        let uid = "@alice:example.com".to_owned();
        let first  = ag.record_free_guess(uid.clone(), make_guess(10.0, 20.0), 1);
        let second = ag.record_free_guess(uid.clone(), make_guess(30.0, 40.0), 1);
        assert!(first,  "first submission must be accepted in one-shot mode");
        assert!(!second, "second submission must be rejected in one-shot mode");
        assert_eq!(guesses_of(&ag)[&uid].lat, 10.0, "original guess must not be overwritten");
    }

    #[test]
    fn unlimited_mode_allows_update() {
        let mut ag = make_game();
        let uid = "@alice:example.com".to_owned();
        ag.record_free_guess(uid.clone(), make_guess(10.0, 20.0), 0);
        let updated = ag.record_free_guess(uid.clone(), make_guess(30.0, 40.0), 0);
        assert!(updated, "update must be accepted in unlimited mode");
        assert_eq!(guesses_of(&ag)[&uid].lat, 30.0, "guess must be updated to latest value");
    }

    #[test]
    fn multiplayer_all_submit_independently() {
        let mut ag = make_game();
        let players = ["@alice:s", "@bob:s", "@carol:s"];
        for (i, uid) in players.iter().enumerate() {
            let accepted = ag.record_free_guess(uid.to_string(), make_guess(i as f64, i as f64), 0);
            assert!(accepted, "{uid} should be accepted");
        }
        assert_eq!(guesses_of(&ag).len(), 3, "all 3 guesses must be recorded");
    }

    #[test]
    fn authorization_wrong_token_not_found() {
        let store = TokenStore::default();
        assert!(store.tokens.get("nonexistent").is_none(), "unknown token must not be found");
    }
}
