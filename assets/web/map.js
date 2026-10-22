"use strict";
// The guess map (src/web.rs serves it). The page's values come as data attributes on
// <body>, so the page needs no inline script (Content-Security-Policy script-src 'self').
(function () {
  var data = document.body.dataset;
  var TOKEN = data.token || '';
  var NAME = data.name || '';
  var ONE_SHOT = data.oneShot === 'true';
  var LANG = data.lang || 'en';

  var T = {
    en:{tap:'Tap the map to place your pin',submit:'Submit guess',submitting:'Submitting…',locked_in:'✅ Locked in: ',locked_btn:'✅ Guess locked in',update:'✏️ Update guess',recorded:'✅ Guess recorded: ',net_err:'❌ Network error: try again',err:'Error: try again',title:'GeoGuessr: place your guess',search_ph:'Search a place or paste lat, lon…',search_btn:'Search',sat:'🛰 Satellite',map:'🗺 Map',show_sat:'Show satellite view',show_map:'Show map view',closed:'⏰ Time is up: this guess has closed.',expired:'⏰ This link has expired.'},
    de:{tap:'Karte antippen, um Stecknadel zu setzen',submit:'Antwort absenden',submitting:'Wird gesendet…',locked_in:'✅ Festgelegt: ',locked_btn:'✅ Antwort festgelegt',update:'✏️ Antwort ändern',recorded:'✅ Antwort gespeichert: ',net_err:'❌ Netzwerkfehler: erneut versuchen',err:'Fehler: erneut versuchen',title:'GeoGuessr: Tipp setzen',search_ph:'Ort suchen oder Breite, Länge einfügen…',search_btn:'Suchen',sat:'🛰 Satellit',map:'🗺 Karte',show_sat:'Satellitenbild zeigen',show_map:'Karte zeigen',closed:'⏰ Die Zeit ist um: Diese Runde ist geschlossen.',expired:'⏰ Dieser Link ist abgelaufen.'},
    fr:{tap:'Appuyez sur la carte pour placer votre repère',submit:'Soumettre',submitting:'Envoi…',locked_in:'✅ Verrouillé : ',locked_btn:'✅ Réponse verrouillée',update:'✏️ Modifier',recorded:'✅ Réponse enregistrée : ',net_err:'❌ Erreur réseau: réessayez',err:'Erreur: réessayez',title:'GeoGuessr : placez votre réponse',search_ph:'Chercher un lieu ou coller lat, lon…',search_btn:'Rechercher',sat:'🛰 Satellite',map:'🗺 Carte',show_sat:'Afficher la vue satellite',show_map:'Afficher la carte',closed:'⏰ Temps écoulé : cette manche est close.',expired:'⏰ Ce lien a expiré.'},
    es:{tap:'Toca el mapa para colocar tu pin',submit:'Enviar respuesta',submitting:'Enviando…',locked_in:'✅ Confirmado: ',locked_btn:'✅ Respuesta confirmada',update:'✏️ Actualizar respuesta',recorded:'✅ Respuesta guardada: ',net_err:'❌ Error de red: inténtalo de nuevo',err:'Error: inténtalo de nuevo',title:'GeoGuessr: coloca tu respuesta',search_ph:'Busca un lugar o pega lat, lon…',search_btn:'Buscar',sat:'🛰 Satélite',map:'🗺 Mapa',show_sat:'Ver satélite',show_map:'Ver mapa',closed:'⏰ Se acabó el tiempo: esta ronda está cerrada.',expired:'⏰ Este enlace ha caducado.'},
    ru:{tap:'Нажмите на карту, чтобы поставить метку',submit:'Отправить ответ',submitting:'Отправка…',locked_in:'✅ Зафиксировано: ',locked_btn:'✅ Ответ зафиксирован',update:'✏️ Изменить ответ',recorded:'✅ Ответ записан: ',net_err:'❌ Ошибка сети: попробуйте снова',err:'Ошибка: попробуйте снова',title:'GeoGuessr: ваш ответ',search_ph:'Найдите место или вставьте шир., долг.…',search_btn:'Искать',sat:'🛰 Спутник',map:'🗺 Карта',show_sat:'Показать спутник',show_map:'Показать карту',closed:'⏰ Время вышло: приём ответов закрыт.',expired:'⏰ Срок действия ссылки истёк.'},
    it:{tap:'Tocca la mappa per posizionare il pin',submit:'Invia risposta',submitting:'Invio…',locked_in:'✅ Bloccato: ',locked_btn:'✅ Risposta bloccata',update:'✏️ Aggiorna risposta',recorded:'✅ Risposta registrata: ',net_err:'❌ Errore di rete: riprova',err:'Errore: riprova',title:'GeoGuessr: piazza la tua risposta',search_ph:'Cerca un luogo o incolla lat, lon…',search_btn:'Cerca',sat:'🛰 Satellite',map:'🗺 Mappa',show_sat:'Mostra satellite',show_map:'Mostra mappa',closed:'⏰ Tempo scaduto: questo turno è chiuso.',expired:'⏰ Questo link è scaduto.'},
    pl:{tap:'Dotknij mapę, aby umieścić pinezkę',submit:'Prześlij odpowiedź',submitting:'Wysyłanie…',locked_in:'✅ Zablokowano: ',locked_btn:'✅ Odpowiedź zablokowana',update:'✏️ Zaktualizuj odpowiedź',recorded:'✅ Odpowiedź zapisana: ',net_err:'❌ Błąd sieci: spróbuj ponownie',err:'Błąd: spróbuj ponownie',title:'GeoGuessr: twoja odpowiedź',search_ph:'Szukaj miejsca lub wklej szer., dł.…',search_btn:'Szukaj',sat:'🛰 Satelita',map:'🗺 Mapa',show_sat:'Pokaż widok satelitarny',show_map:'Pokaż mapę',closed:'⏰ Czas minął: ta runda jest zamknięta.',expired:'⏰ Ten link wygasł.'},
    nl:{tap:'Tik op de kaart om je pin te plaatsen',submit:'Antwoord verzenden',submitting:'Verzenden…',locked_in:'✅ Vergrendeld: ',locked_btn:'✅ Antwoord vergrendeld',update:'✏️ Antwoord bijwerken',recorded:'✅ Antwoord opgeslagen: ',net_err:'❌ Netwerkfout: probeer opnieuw',err:'Fout: probeer opnieuw',title:'GeoGuessr: plaats je antwoord',search_ph:'Zoek een plaats of plak lat, lon…',search_btn:'Zoeken',sat:'🛰 Satelliet',map:'🗺 Kaart',show_sat:'Satellietbeeld tonen',show_map:'Kaart tonen',closed:'⏰ De tijd is om: deze ronde is gesloten.',expired:'⏰ Deze link is verlopen.'},
    pt:{tap:'Toque no mapa para colocar seu marcador',submit:'Enviar resposta',submitting:'Enviando…',locked_in:'✅ Confirmado: ',locked_btn:'✅ Resposta confirmada',update:'✏️ Atualizar resposta',recorded:'✅ Resposta registada: ',net_err:'❌ Erro de rede: tente novamente',err:'Erro: tente novamente',title:'GeoGuessr: coloque a sua resposta',search_ph:'Procure um local ou cole lat, lon…',search_btn:'Procurar',sat:'🛰 Satélite',map:'🗺 Mapa',show_sat:'Mostrar satélite',show_map:'Mostrar mapa',closed:'⏰ O tempo acabou: esta ronda está fechada.',expired:'⏰ Esta ligação expirou.'},
    uk:{tap:'Натисніть на карту, щоб поставити мітку',submit:'Надіслати відповідь',submitting:'Надсилання…',locked_in:'✅ Зафіксовано: ',locked_btn:'✅ Відповідь зафіксована',update:'✏️ Оновити відповідь',recorded:'✅ Відповідь збережено: ',net_err:'❌ Помилка мережі: спробуйте ще раз',err:'Помилка: спробуйте ще раз',title:'GeoGuessr: ваша відповідь',search_ph:'Знайдіть місце або вставте шир., довг.…',search_btn:'Шукати',sat:'🛰 Супутник',map:'🗺 Карта',show_sat:'Показати супутник',show_map:'Показати карту',closed:'⏰ Час вийшов: прийом відповідей закрито.',expired:'⏰ Термін дії посилання минув.'},
    ja:{tap:'地図をタップしてピンを置く',submit:'回答を送信',submitting:'送信中…',locked_in:'✅ 確定: ',locked_btn:'✅ 回答を確定',update:'✏️ 回答を更新',recorded:'✅ 回答を記録: ',net_err:'❌ ネットワークエラー: 再試行',err:'エラー: 再試行',title:'GeoGuessr: 回答を置く',search_ph:'場所を検索、または緯度, 経度を貼り付け…',search_btn:'検索',sat:'🛰 衛星',map:'🗺 地図',show_sat:'衛星写真を表示',show_map:'地図を表示',closed:'⏰ 時間切れ: この問題は締め切られました。',expired:'⏰ このリンクは期限切れです。'},
    zh:{tap:'点击地图放置图钉',submit:'提交答案',submitting:'提交中…',locked_in:'✅ 已确认: ',locked_btn:'✅ 答案已确认',update:'✏️ 更新答案',recorded:'✅ 答案已记录: ',net_err:'❌ 网络错误: 请重试',err:'错误: 请重试',title:'GeoGuessr：放置你的答案',search_ph:'搜索地点或粘贴纬度, 经度…',search_btn:'搜索',sat:'🛰 卫星',map:'🗺 地图',show_sat:'显示卫星图',show_map:'显示地图',closed:'⏰ 时间到：本题已关闭。',expired:'⏰ 此链接已过期。'},
    ar:{tap:'اضغط على الخريطة لوضع الدبوس',submit:'إرسال الإجابة',submitting:'جارٍ الإرسال…',locked_in:'✅ تم التأكيد: ',locked_btn:'✅ تم تأكيد الإجابة',update:'✏️ تحديث الإجابة',recorded:'✅ تم تسجيل الإجابة: ',net_err:'❌ خطأ في الشبكة: حاول مرة أخرى',err:'خطأ: حاول مرة أخرى',title:'GeoGuessr: ضع إجابتك',search_ph:'ابحث عن مكان أو الصق خط العرض، خط الطول…',search_btn:'بحث',sat:'🛰 قمر صناعي',map:'🗺 خريطة',show_sat:'عرض صور الأقمار الصناعية',show_map:'عرض الخريطة',closed:'⏰ انتهى الوقت: أُغلق هذا التخمين.',expired:'⏰ انتهت صلاحية هذا الرابط.'},
    tr:{tap:'Pini yerleştirmek için haritaya dokun',submit:'Tahmin gönder',submitting:'Gönderiliyor…',locked_in:'✅ Kilitlendi: ',locked_btn:'✅ Tahmin kilitlendi',update:'✏️ Tahmini güncelle',recorded:'✅ Tahmin kaydedildi: ',net_err:'❌ Ağ hatası: tekrar dene',err:'Hata: tekrar dene',title:'GeoGuessr: tahminini yerleştir',search_ph:'Yer ara veya enlem, boylam yapıştır…',search_btn:'Ara',sat:'🛰 Uydu',map:'🗺 Harita',show_sat:'Uydu görünümünü göster',show_map:'Haritayı göster',closed:'⏰ Süre doldu: bu tahmin kapandı.',expired:'⏰ Bu bağlantının süresi doldu.'},
    sv:{tap:'Tryck på kartan för att placera din nål',submit:'Skicka svar',submitting:'Skickar…',locked_in:'✅ Låst: ',locked_btn:'✅ Svar låst',update:'✏️ Uppdatera svar',recorded:'✅ Svar registrerat: ',net_err:'❌ Nätverksfel: försök igen',err:'Fel: försök igen',title:'GeoGuessr: placera ditt svar',search_ph:'Sök en plats eller klistra in lat, lon…',search_btn:'Sök',sat:'🛰 Satellit',map:'🗺 Karta',show_sat:'Visa satellitbild',show_map:'Visa karta',closed:'⏰ Tiden är ute: den här gissningen är stängd.',expired:'⏰ Länken har gått ut.'},
    fi:{tap:'Napauta karttaa asettaaksesi nuppineulan',submit:'Lähetä arvaus',submitting:'Lähetetään…',locked_in:'✅ Lukittu: ',locked_btn:'✅ Arvaus lukittu',update:'✏️ Päivitä arvaus',recorded:'✅ Arvaus tallennettu: ',net_err:'❌ Verkkovirhe: yritä uudelleen',err:'Virhe: yritä uudelleen',title:'GeoGuessr: aseta arvauksesi',search_ph:'Hae paikkaa tai liitä lat, lon…',search_btn:'Hae',sat:'🛰 Satelliitti',map:'🗺 Kartta',show_sat:'Näytä satelliittikuva',show_map:'Näytä kartta',closed:'⏰ Aika loppui: tämä arvaus on suljettu.',expired:'⏰ Linkki on vanhentunut.'},
    da:{tap:'Tryk på kortet for at placere din nål',submit:'Send gæt',submitting:'Sender…',locked_in:'✅ Låst: ',locked_btn:'✅ Gæt låst',update:'✏️ Opdater gæt',recorded:'✅ Gæt registreret: ',net_err:'❌ Netværksfejl: prøv igen',err:'Fejl: prøv igen',title:'GeoGuessr: placer dit gæt',search_ph:'Søg et sted eller indsæt lat, lon…',search_btn:'Søg',sat:'🛰 Satellit',map:'🗺 Kort',show_sat:'Vis satellitbillede',show_map:'Vis kort',closed:'⏰ Tiden er gået: dette gæt er lukket.',expired:'⏰ Linket er udløbet.'},
    cs:{tap:'Klepněte na mapu a umístěte špendlík',submit:'Odeslat odpověď',submitting:'Odesílám…',locked_in:'✅ Uzamčeno: ',locked_btn:'✅ Odpověď uzamčena',update:'✏️ Aktualizovat odpověď',recorded:'✅ Odpověď zaznamenána: ',net_err:'❌ Chyba sítě: zkuste znovu',err:'Chyba: zkuste znovu',title:'GeoGuessr: umístěte odpověď',search_ph:'Hledejte místo nebo vložte šířku, délku…',search_btn:'Hledat',sat:'🛰 Satelit',map:'🗺 Mapa',show_sat:'Zobrazit satelit',show_map:'Zobrazit mapu',closed:'⏰ Čas vypršel: tento tip je uzavřen.',expired:'⏰ Platnost odkazu vypršela.'},
    hu:{tap:'Koppintson a térképre a gombostű elhelyezéséhez',submit:'Tipp elküldése',submitting:'Küldés…',locked_in:'✅ Rögzítve: ',locked_btn:'✅ Tipp rögzítve',update:'✏️ Tipp frissítése',recorded:'✅ Tipp rögzítve: ',net_err:'❌ Hálózati hiba: próbálja újra',err:'Hiba: próbálja újra',title:'GeoGuessr: tedd le a tipped',search_ph:'Keress helyet, vagy illessz be szélességet, hosszúságot…',search_btn:'Keresés',sat:'🛰 Műhold',map:'🗺 Térkép',show_sat:'Műholdkép mutatása',show_map:'Térkép mutatása',closed:'⏰ Lejárt az idő: ez a tipp lezárult.',expired:'⏰ Ez a link lejárt.'},
    ro:{tap:'Atinge harta pentru a plasa acul',submit:'Trimite răspuns',submitting:'Se trimite…',locked_in:'✅ Blocat: ',locked_btn:'✅ Răspuns blocat',update:'✏️ Actualizează răspuns',recorded:'✅ Răspuns înregistrat: ',net_err:'❌ Eroare de rețea: încearcă din nou',err:'Eroare: încearcă din nou',title:'GeoGuessr: plasează răspunsul',search_ph:'Caută un loc sau lipește lat, lon…',search_btn:'Caută',sat:'🛰 Satelit',map:'🗺 Hartă',show_sat:'Arată satelitul',show_map:'Arată harta',closed:'⏰ Timpul a expirat: această rundă s-a închis.',expired:'⏰ Acest link a expirat.'},
    el:{tap:'Πατήστε στον χάρτη για να τοποθετήσετε την καρφίτσα',submit:'Αποστολή απάντησης',submitting:'Αποστολή…',locked_in:'✅ Κλειδωμένο: ',locked_btn:'✅ Απάντηση κλειδωμένη',update:'✏️ Ενημέρωση απάντησης',recorded:'✅ Απάντηση καταγράφηκε: ',net_err:'❌ Σφάλμα δικτύου: δοκιμάστε ξανά',err:'Σφάλμα: δοκιμάστε ξανά',title:'GeoGuessr: βάλτε την απάντησή σας',search_ph:'Αναζητήστε μέρος ή επικολλήστε πλάτος, μήκος…',search_btn:'Αναζήτηση',sat:'🛰 Δορυφόρος',map:'🗺 Χάρτης',show_sat:'Δορυφορική προβολή',show_map:'Προβολή χάρτη',closed:'⏰ Ο χρόνος τελείωσε: η απάντηση έκλεισε.',expired:'⏰ Ο σύνδεσμος έληξε.'},
    he:{tap:'גע במפה כדי למקם את הסיכה',submit:'שלח תשובה',submitting:'שולח…',locked_in:'✅ נעול: ',locked_btn:'✅ תשובה נעולה',update:'✏️ עדכן תשובה',recorded:'✅ תשובה נרשמה: ',net_err:'❌ שגיאת רשת: נסה שוב',err:'שגיאה: נסה שוב',title:'GeoGuessr: מקמו את התשובה',search_ph:'חפשו מקום או הדביקו קו רוחב, קו אורך…',search_btn:'חיפוש',sat:'🛰 לוויין',map:'🗺 מפה',show_sat:'הצגת תצלום לוויין',show_map:'הצגת מפה',closed:'⏰ הזמן נגמר: הניחוש נסגר.',expired:'⏰ פג תוקף הקישור.'},
    ko:{tap:'지도를 눌러 핀을 놓으세요',submit:'답안 제출',submitting:'제출 중…',locked_in:'✅ 확정됨: ',locked_btn:'✅ 답안 확정',update:'✏️ 답안 수정',recorded:'✅ 답안 기록됨: ',net_err:'❌ 네트워크 오류: 다시 시도',err:'오류: 다시 시도',title:'GeoGuessr: 답 놓기',search_ph:'장소를 검색하거나 위도, 경도를 붙여넣기…',search_btn:'검색',sat:'🛰 위성',map:'🗺 지도',show_sat:'위성 보기',show_map:'지도 보기',closed:'⏰ 시간 종료: 이 문제는 마감되었습니다.',expired:'⏰ 이 링크는 만료되었습니다.'},
    th:{tap:'แตะแผนที่เพื่อวางหมุด',submit:'ส่งคำตอบ',submitting:'กำลังส่ง…',locked_in:'✅ ล็อคแล้ว: ',locked_btn:'✅ ล็อคคำตอบแล้ว',update:'✏️ อัปเดตคำตอบ',recorded:'✅ บันทึกคำตอบแล้ว: ',net_err:'❌ เครือข่ายผิดพลาด: ลองอีกครั้ง',err:'ผิดพลาด: ลองอีกครั้ง',title:'GeoGuessr: วางคำตอบของคุณ',search_ph:'ค้นหาสถานที่หรือวางละติจูด, ลองจิจูด…',search_btn:'ค้นหา',sat:'🛰 ดาวเทียม',map:'🗺 แผนที่',show_sat:'แสดงภาพดาวเทียม',show_map:'แสดงแผนที่',closed:'⏰ หมดเวลา: ปิดรับคำตอบแล้ว',expired:'⏰ ลิงก์นี้หมดอายุแล้ว'},
    vi:{tap:'Chạm vào bản đồ để đặt ghim',submit:'Gửi đoán',submitting:'Đang gửi…',locked_in:'✅ Đã xác nhận: ',locked_btn:'✅ Đã khóa đoán',update:'✏️ Cập nhật đoán',recorded:'✅ Đoán đã được ghi: ',net_err:'❌ Lỗi mạng: thử lại',err:'Lỗi: thử lại',title:'GeoGuessr: đặt câu trả lời',search_ph:'Tìm địa điểm hoặc dán vĩ độ, kinh độ…',search_btn:'Tìm',sat:'🛰 Vệ tinh',map:'🗺 Bản đồ',show_sat:'Xem ảnh vệ tinh',show_map:'Xem bản đồ',closed:'⏰ Hết giờ: lượt đoán này đã đóng.',expired:'⏰ Liên kết này đã hết hạn.'},
    id:{tap:'Ketuk peta untuk meletakkan pin',submit:'Kirim tebakan',submitting:'Mengirim…',locked_in:'✅ Dikunci: ',locked_btn:'✅ Tebakan dikunci',update:'✏️ Perbarui tebakan',recorded:'✅ Tebakan dicatat: ',net_err:'❌ Kesalahan jaringan: coba lagi',err:'Kesalahan: coba lagi',title:'GeoGuessr: letakkan tebakanmu',search_ph:'Cari tempat atau tempel lat, lon…',search_btn:'Cari',sat:'🛰 Satelit',map:'🗺 Peta',show_sat:'Tampilkan satelit',show_map:'Tampilkan peta',closed:'⏰ Waktu habis: tebakan ini sudah ditutup.',expired:'⏰ Tautan ini sudah kedaluwarsa.'}
  };

  var lang = LANG.split('-')[0];

  function t(key) {
    var row = T[lang] || T.en;
    return row[key] || T.en[key] || key;
  }

  document.title = t('title');
  if (NAME) document.getElementById('name').textContent = '👤 ' + NAME;

  var searchInput = document.getElementById('search-input');
  var searchBtn = document.getElementById('search-btn');
  searchInput.placeholder = t('search_ph');
  searchInput.setAttribute('aria-label', t('search_ph'));
  searchBtn.setAttribute('aria-label', t('search_btn'));
  searchBtn.title = t('search_btn');

  var LIBERTY = 'https://tiles.openfreemap.org/styles/liberty';
  var SATELLITE = {
    version: 8,
    sources: {sat: {type:'raster', tileSize:256, attribution:'© Esri',
      tiles:['https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}']}},
    layers: [{id:'sat', type:'raster', source:'sat'}]
  };
  var isSatellite = false;

  var map = new maplibregl.Map({
    container: 'map',
    style: LIBERTY,
    center: [0, 20],
    zoom: 2,
  });

  function applyLanguage() {
    map.getStyle().layers.forEach(function (layer) {
      if (layer.layout && layer.layout['text-field']) {
        map.setLayoutProperty(layer.id, 'text-field', [
          'coalesce',
          ['get', 'name:' + lang],
          ['get', 'name:en'],
          ['get', 'name'],
        ]);
      }
    });
  }

  map.on('style.load', function () { if (!isSatellite) applyLanguage(); });

  var toggleBtn = document.getElementById('layer-toggle');
  function labelToggle() {
    toggleBtn.textContent = isSatellite ? t('map') : t('sat');
    toggleBtn.setAttribute('aria-label', isSatellite ? t('show_map') : t('show_sat'));
  }
  labelToggle();
  toggleBtn.addEventListener('click', function () {
    isSatellite = !isSatellite;
    map.setStyle(isSatellite ? SATELLITE : LIBERTY);
    labelToggle();
  });

  var hint = document.getElementById('hint');
  var btn = document.getElementById('btn');
  var pin = null;
  var pinLat = null;
  var pinLon = null;
  var submitted = false;

  hint.textContent = t('tap');
  btn.textContent = t('submit');

  function placePin(lat, lon) {
    pinLat = lat;
    pinLon = lon;
    if (pin) {
      pin.setLngLat([lon, lat]);
    } else {
      pin = new maplibregl.Marker({draggable: true})
        .setLngLat([lon, lat])
        .addTo(map);
      pin.on('dragend', function () {
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

  function handleClick(e) {
    if (submitted && ONE_SHOT) return;
    placePin(e.lngLat.lat, e.lngLat.lng);
  }
  map.on('click', handleClick);

  async function doSearch() {
    var q = searchInput.value.trim();
    if (!q) return;
    var m = q.match(/^(-?\d+\.?\d*)[,\s]+(-?\d+\.?\d*)$/);
    if (m) {
      var lat = parseFloat(m[1]), lon = parseFloat(m[2]);
      if (lat >= -90 && lat <= 90 && lon >= -180 && lon <= 180) {
        map.flyTo({center: [lon, lat], zoom: 12});
        placePin(lat, lon);
        return;
      }
    }
    try {
      var url = 'https://nominatim.openstreetmap.org/search?q=' + encodeURIComponent(q) +
                '&format=json&limit=1&accept-language=' + encodeURIComponent(lang);
      var r = await fetch(url);
      var d = await r.json();
      if (d && d[0]) {
        map.flyTo({center: [parseFloat(d[0].lon), parseFloat(d[0].lat)], zoom: 10});
        placePin(parseFloat(d[0].lat), parseFloat(d[0].lon));
      }
    } catch (_) {}
  }
  searchBtn.addEventListener('click', doSearch);
  searchInput.addEventListener('keydown', function (e) { if (e.key === 'Enter') doSearch(); });

  function setReady() {
    btn.disabled = false;
    btn.className = 'ready';
    btn.textContent = submitted ? t('update') : t('submit');
  }

  btn.addEventListener('click', async function () {
    if (pinLat === null || btn.disabled) return;
    btn.disabled = true;
    btn.textContent = t('submitting');
    try {
      var r = await fetch('/g/' + encodeURIComponent(TOKEN) + '/submit', {
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
      } else if (r.status === 410) {
        hint.textContent = t('closed');
      } else if (r.status === 404) {
        hint.textContent = t('expired');
      } else {
        hint.textContent = '❌ ' + t('err');
        setReady();
      }
    } catch (e) {
      hint.textContent = t('net_err');
      setReady();
    }
  });

  function fmt(lat, lon) { return lat.toFixed(3) + ', ' + lon.toFixed(3); }
})();
