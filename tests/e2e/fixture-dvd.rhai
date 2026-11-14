fn attribute(value) {
    value = regex::replace_all("&", value, "&amp;");
    value = regex::replace_all("\"", value, "&quot;");
    value = regex::replace_all("<", value, "&lt;");
    regex::replace_all(">", value, "&gt;")
}

let xml = "<dvdauthor><vmgm/><titleset><titles>"
    + "<video format=\"ntsc\" aspect=\"4:3\"/>"
    + "<audio format=\"ac3\" lang=\"en\"/>"
    + "<pgc><vob file=\"" + attribute(long_file) + "\"/></pgc>"
    + "<pgc><vob file=\"" + attribute(short_file) + "\"/></pgc>"
    + "</titles></titleset></dvdauthor>";
fs::write_text(MOUNTS[0], xml);
