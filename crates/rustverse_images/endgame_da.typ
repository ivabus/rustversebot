#import "prelude.typ": conf

#show: conf.with(mode: "da", size: "big")

#let input = json("input.json");

#align(center)[
  #image("images/da.webp", width: 33%)
  #input.name, id: #input.id
  // надо дополнять input
  #input.begin_time.split(" ").first() -
  #input.end_time.split(" ").first()
]

#for (n, mode) in input.modes.enumerate() {
  for (k, (id, room)) in mode.zone.pairs().enumerate() {
    rect(stroke: if mode.zone_type == 1002 { red } else { white }, width: 100%, radius: 10pt)[
      #set text(size: 8pt)
      #text(size: 11pt, baseline: 2pt)[#room.name]
      #let elements = room.layer_room.values().first().monster_list.values().first().element
      #if elements.values().contains(-1) or elements.values().contains(1) [
        #columns(2)[
          #if elements.filter(x => x == 1).len() != 0 [
            Weakness:
            #for element in elements.filter(x => x == 1).keys().intersperse(", ") [
              #element
            ]
          ]
          #colbreak()
          #if elements.filter(x => x == -1).len() != 0 [
            Resistance:
            #for element in elements.filter(x => x == -1).keys().intersperse(", ") [
              #element
            ]
          ]
        ]
      ]

      #let stats = room.layer_room.values().first().monster_list.values().first().stats
      HP: #stats.at("hp") #h(0.5cm) ATK: #stats.at("attack") #h(0.5cm) DEF: #stats.at("defence")


      #for buff in room.layer_buff.values().filter(x => x.desc.len() != 0) [
        #let lines = buff.desc.replace("\n", "").replace("·", "\n·").trim()
        #lines\
      ]

    ]
  }
}

#rect(width: 100%, radius: 10pt)[
  #let buffs = input.modes.first().zone.values().first().selectable_buff;
  #for (id, buff) in buffs.pairs() [

    #text(size: 11pt)[#buff.title]

    #set text(size: 8pt)
    #buff.desc
    #v(0.1cm)

    #if buff != buffs.values().last() [
      #line(length: 100%, stroke: white)
      #colbreak()
    ]
  ]
]
