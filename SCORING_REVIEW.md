# GeoGuessr scoring review

## Kurzbefund

Die beobachtete Unfairness ist vor allem ein **Erklärungs- und Zielkonflikt im Ranking**, kein klarer Rechenfehler: Die sichtbare Rangliste ist keine „wer war am häufigsten Erster“-Tabelle. Sie sortiert nach einem geglätteten Durchschnittsrating. Einzelne erste Plätze sind darin nicht als Siege erfasst. Zusätzlich zählt ein verpasster Tipp bei Personen, die der Runde beigetreten sind, als 0 Punkte. Wer gar nicht beitritt, ist dagegen nicht Teil der Runde und wird nicht gewertet.

## Was der Code aktuell macht

1. **Punkte pro Tipp:** `5000 × exp(-Distanz_km / score_half_life_km)`, auf ganze Punkte abgerundet. Der Code-Default für `score_half_life_km` ist 2000 km. Bei diesem Default ergeben sich ungefähr:
   - 50 km → 4.876 Punkte
   - 500 km → 3.894 Punkte
   - 1.000 km → 3.032 Punkte
   - 2.000 km → 1.839 Punkte
2. **Rundenplatz:** Innerhalb eines Bildes sortiert der Bot zuerst nach Punkten, danach bei Punktgleichstand nach der exakten Entfernung. Dadurch kann die Platzierung bei dicht beieinanderliegenden Tipps tatsächlich von Metern abhängen, obwohl die ganzzahligen Punkte gleich sind.
3. **Gesamt-Rangliste:** `!leaderboard`/Monatsrangliste sortiert nach `rating`, nicht nach Siegen oder Gesamtpunkten. Das Rating ist ein Shrinkage-Durchschnitt: eigener Durchschnitt, mit `k` virtuellen Tipps zur Community-Baseline gemischt. Der Default ist `k=15`; unter 10 Tipps erscheint 🔰, aber das ändert die Wertung nicht.
4. **Auslassen:** Beigetretene Spieler:innen, die ein Bild auslassen, bekommen dafür einen expliziten Null-Punkte-Eintrag. Das senkt den Durchschnitt und die Completion-Rate. Personen außerhalb der Beitrittsliste werden nicht als „missed“ gezählt.
5. **Distanzanzeige:** Unter 1 km zeigt der Bot Meter an; unter 50 m steht in der Ergebnisliste „exact“. Das ist nur Anzeige: die Wertung nutzt weiterhin die exakte Distanz und eine ganzzahlige Punktzahl.

Der konfigurierte Wert im echten Lauf kann vom Code-Default abweichen; geprüft wurde hier der Quellcode und `config.example.toml`, nicht die Live-Konfiguration.

## Warum Tini oft gewinnt, aber nicht führt

Ein erster Platz ist momentan nur die beste Einzelplatzierung im jeweiligen Bild. Es gibt dafür **keinen Bonuspunkt**. Ein:e Spieler:in kann häufig knapp vor allen anderen liegen, aber insgesamt weniger Punkte pro Tipp erzielen oder mehr 0-Punkte-Auslassungen haben. Umgekehrt kann jemand selten gewinnen, aber konstant hohe Punktzahlen erreichen.

Das Shrinkage-Rating verstärkt den Effekt bei wenigen Tipps: Bei `k=15` zählt die Community-Baseline stark mit, bis viele Tipps gesammelt sind. Das ist als Schutz vor einem einzelnen Glückstreffer gedacht, wirkt aber für eine lockere Haus-Rangliste schwer nachvollziehbar. Monats- und Allzeit-Rangliste verwenden dieselbe Ratinglogik; der Spielabschluss zeigt zusätzlich die Rundensumme als „pts/guess“.

## Kritische Findings

### Hoch – Ranglisten-Ziel ist für Nutzer:innen nicht klar und passt nicht zu „oft Erste“

Die Rangliste heißt Leaderboard und zeigt Medaillen, aber ihr Schlüssel ist ein geschrumpfter Durchschnitt statt Siege, Rundensiege oder aufsummierte Punkte. Dadurch ist Tinis Beobachtung plausibel und kein Widerspruch im Code. Die Legende erklärt die Ratingformel nur abstrakt; außerdem ist die Provisional-Markierung rein informativ.

**Vorschlag:** Die Haupt-Rangliste auf ein leicht verständliches Saison-/Monatspunktesystem umstellen: pro Bild Platzierungspunkte vergeben (beispielsweise 5/3/2/1 für die ersten vier, plus Teilnahme-Punkt für einen gültigen Tipp), über die Runde bzw. den Monat aufsummieren. Tiebreaker: Gesamt-Geoguessr-Punkte, danach mittlere Distanz. Ergänzend kann eine separate „Ø Punkte pro Tipp“-Skill-Tabelle erhalten bleiben, mit Mindestanzahl beantworteter Tipps und klarer Anzeige „x/y mitgespielt“. Dann bedeutet „Erste“ auch sichtbar etwas, ohne Können und Aktivität zu vermischen.

Falls keine zweite Tabelle gewünscht ist: das bestehende Rating beibehalten, aber sichtbarer als „Skill-Rating (Durchschnitt, kleine Stichprobe geglättet)“ benennen und in der Rangliste Siege, beantwortete/verpasste Tipps sowie rohe Durchschnittspunkte zeigen.

### Hoch – Nullwertung bei Auslassen macht eine lockere Runde unnötig strafend

Ein Beitritt zählt als Zusage für jedes Bild der Runde. Wer nach dem Beitritt nicht mehr miträt, bekommt Nullpunkte und verschlechtert sein Rating; ein verspäteter Einstieg wird dagegen nicht rückwirkend belastet. Das ist konsistent implementiert, aber bei Alltagssituationen (DM nicht gesehen, beschäftigt, Verbindung weg) hart und für Spieler:innen nicht offensichtlich.

**Vorschlag:** Standardmäßig nur gültige Tipps in den Skill-Durchschnitt aufnehmen; Completion separat anzeigen. Für eine Wettbewerbs-/Saisonwertung Teilnahme-Punkte nur bei tatsächlichem Tipp vergeben. Optional eine explizite „Ich spiele diese Runde mit“-Bestätigung beibehalten und Nullwertung nur für klar zugesagte, danach ausgelassene Bilder anwenden. Mindestens die Join-Nachricht sollte sagen, dass ein Beitritt die ganze Runde umfasst und Auslassen 0 zählt.

### Mittel – Meter-Genauigkeit ist als Tiebreaker psychologisch zu fein

Bei 2000 km Halbwertsdistanz ist die Punktkurve nahe am Ziel sehr flach: 500 m ergeben rund 4.998 Punkte, 1 km rund 4.997. Die abgeschnittenen Ganzzahlen produzieren viele Punktgleichstände; danach entscheidet intern die ungerundete Entfernung. Gleichzeitig wird die Entfernung bis auf ganze Meter angezeigt. Daher entsteht der Eindruck, ein „besserer“ Tipp sei nur wenige Meter besser, obwohl die Punkte praktisch gleich sind.

**Vorschlag:** Meter nicht als versteckten Rang-Tiebreaker verwenden. Bei gleichen Punkten Gleichstand anzeigen oder auf eine sinnvolle Distanzstufe (z.B. 100 m bzw. 1 km, abhängig vom Bereich) runden. In Ergebnissen Distanz in groben, gut lesbaren Stufen darstellen; „🎯 sehr nah“/„<1 km“ statt scheinbarer Präzision, wo die Bildposition selbst ungenau ist.

### Mittel – Einzelbild-Sieg, Punktergebnis und langfristige Skill-Wertung werden vermischt

Der Reveal zeigt Rang anhand von Punktzahl und exakter Distanz, die Rundenzusammenfassung summiert Punkte, während die globale Rangliste ein Bayesian-artiges Durchschnittsrating nutzt. Diese drei Ordnungen sind verschieden, obwohl dieselben Medaillen-/Leaderboard-Signale verwendet werden.

**Vorschlag:** Drei klar benannte Größen trennen: „Bildplatzierung“ (mit Gleichständen), „Runden-/Monatspunkte“ (Wettbewerb) und optional „Ø Punkte pro Tipp“ (Skill). Den automatischen Abschlussbeitrag auf eine dieser Hauptwertungen ausrichten; die anderen als Nebenstatistik zeigen.

## Empfohlene einfache Zielregel

Für eine freundschaftliche, wiederkehrende Haus-Runde würde ich **Monatspunkte aus Platzierungen** als Hauptwertung nehmen, weil sie Tinis gewünschte Anerkennung für häufige erste Plätze direkt abbildet. Pro Bild: 5/3/2/1 Punkte für Platz 1–4, jeder gültige Tipp erhält zusätzlich 1 Teilnahme-Punkt. Bei geteiltem Platz werden die Punkte der belegten Plätze gemittelt (zwei gemeinsame Erste erhalten je 4 Platzierungspunkte), damit Rundungs-/Distanzdetails niemanden benachteiligen. Fehlende Tipps geben keine Skill-Nullwertung; sie bekommen schlicht keine Platzierungs- oder Teilnahme-Punkte. Daneben bleibt die mittlere Geoguessr-Punktzahl pro gültigem Tipp als Skill-Statistik sichtbar.

Diese Wertung belohnt sowohl Mitmachen als auch gute Platzierungen, macht häufige Siege sichtbar und vermeidet, dass ein verpasster Tipp eine andere Kennzahl heimlich herunterzieht. Wenn absichtliches Auslassen strategisch relevant wird, kann man stattdessen Teilnahme nur bei vorheriger Zusage verpflichtend machen, sollte diese Regel aber deutlich ankündigen.

## Dateien / relevante Stellen

- `src/game.rs`: Distanzwertung, Tie-Breaks, Eintragung verpasster Tipps und Rundenzusammenfassung.
- `src/commands.rs`: Shrinkage-Rating, Ranglisten-Sortierung und Anzeige.
- `src/config.rs`: Defaults `score_half_life_km=2000`, `rating.k=15`, provisional threshold 10.
- `src/db.rs`: Aggregation von Gesamtpunkten, beantworteten und verpassten Tipps für Monats-, 90-Tage- und Allzeit-Ranglisten.

Keine Bot-Logik wurde in diesem Review verändert. Es wurden keine Live-Daten oder die laufende Unraid-Instanz geprüft.
